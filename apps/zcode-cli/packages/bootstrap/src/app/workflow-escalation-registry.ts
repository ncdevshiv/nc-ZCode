// ============================================================
// Workflow 升级问答的停驻注册表（driver 与 run service 之间的唯一连线）
// ============================================================
// actor 的 `escalate` 停驻在 driver 里（它持有 ask 轮次
// 上下文），而作答入口 `resolveQuestion` 在 run service 上（它是端口的持有者）——两者之间需要
// 一张按 qid 查的表。本文件就是那张表，且只是那张表：无 I/O、无持久化、无引擎知识。
//
// 为什么不把 driver 实例交给 service：driver 由 `makeDriver(sink)` 在 `runWorkflowScript`
// **内部**构造，service 从来看不到它。传一张表进去，比为了拿到实例而在 launch 里加一个捕获
// 钩子要少一处时序（表在 launch 之前就存在，注册与查询天然同生命周期）。
//
// **纯内存，与停驻的 deferred 同命**。刻意不持久化：进程亡故后 deferred 已死，一张持久化的
// pending 表只会说谎（它会让主代理去回答一个再也没有人在等的问题）。自愈靠 resume——那个
// ask 会 live 重跑，actor 重新提问并得到一个新 qid，而陈旧 id 拿到结构化拒绝。
//
// 作用域是**每个 run service 实例一张**（≈ 每个 app 会话），而不是模块级单例：跨会话共享一张
// 表等于开一个跨会话的应答洞，与本仓既有的「枚举面只看本会话」同一条纪律。一张表跨本服务名下
// **All on the fly**, this is why qid must be globally unique.

import type {
  DynamicWorkflowResolveQuestionResult,
  DynamicWorkflowRunPendingQuestion,
} from "@zcode/contracts";

/**
* The number of memory items for the exited qid. **Bounded**: A long-distance conversation can have questions and answers any number of times, and the unbounded history table is one
* Silently growing memory. After being evicted `resolveQuestion` will just assign it to `unknown_question` - that's honest
* Degenerate ("I don't know this id"), rather than a wrong answer.
 */
const RETIRED_HISTORY_LIMIT = 256;

/** Facts required for a park registration (provided by driver; qid has been cast by driver). */
export interface ParkedQuestionInput {
  qid: string;
  runId: string;
/** The actor asking the question, in `refToString` form (such as `actor#1@1`). */
  actor: string;
/** The valid name of the actor (`"poet"` of `agent("poet")`); the anonymous actor is absent and does not generate a cover label. */
  actorName?: string;
  question: string;
  context?: string;
  askedAt: number;
}

/** The reason why a qid exits: determines which structured rejection is received by the subsequent resolve. */
type RetiredReason = "resolved" | "withdrawn";

export interface WorkflowEscalationRegistry {
  /**
* Register a pending issue. `settle` is provided by the driver and unwinds the deferred when the answer is received.
   *
* The caller must first use {@link isTaken} to ensure that the qid is not occupied (the driver's casting loop does this) -
* Repeated registration will silently overwrite the previous stop, which is the cause of mismatched answers.
   */
  park(entry: ParkedQuestionInput, settle: (answer: string) => void): void;
/** Whether qid is occupied (parked or retired). The driver casts the qid according to which it is guaranteed to be globally unique. */
  isTaken(qid: string): boolean;
  /**
* Remove a dock item without **answering**: Ask is canceled / called by driver when turn fails (deferred side
* Rejected by the driver itself). Subsequent resolves to that qid result in `run_not_in_flight`.
   */
  withdraw(qid: string): void;
/** Settle a docked item. The three types of structured rejections each state the current situation and next steps. */
  resolve(qid: string, answer: string): DynamicWorkflowResolveQuestionResult;
/** The questions parked on a certain run at this moment, in the order in which they are asked. The projection source of snapshot `pendingQuestions`. */
  pendingFor(runId: string): DynamicWorkflowRunPendingQuestion[];
}

interface ParkedQuestion extends ParkedQuestionInput {
  settle: (answer: string) => void;
}

export function createWorkflowEscalationRegistry(): WorkflowEscalationRegistry {
// The insertion order of Map is the question order (pendingFor directly relies on it and does not save the sequence number separately).
  const parked = new Map<string, ParkedQuestion>();
  const retired = new Map<string, RetiredReason>();

  const retire = (qid: string, reason: RetiredReason): void => {
    parked.delete(qid);
    retired.set(qid, reason);
    while (retired.size > RETIRED_HISTORY_LIMIT) {
// The iteration order of Map is insertion order, so the first key is the oldest one.
      const oldest = retired.keys().next();
      if (oldest.done === true) break;
      retired.delete(oldest.value);
    }
  };

  return {
    park(entry, settle) {
      parked.set(entry.qid, { ...entry, settle });
    },

    isTaken(qid) {
      return parked.has(qid) || retired.has(qid);
    },

    withdraw(qid) {
// Only meaningful for docked items: resolved qid should not be downgraded to `run_not_in_flight`
// (That would rewrite "the answer has been delivered" to "no one is waiting", which are two different facts for the main agent).
      if (!parked.has(qid)) return;
      retire(qid, "withdrawn");
    },

    resolve(qid, answer) {
      const entry = parked.get(qid);
      if (entry === undefined) {
        const reason = retired.get(qid);
        if (reason === "resolved") {
          return {
            ok: false,
            reason: "already_resolved",
            message:
              `Question ${qid} was already answered and the subagent has moved on with that ` +
              `answer. No need to answer again; if you have more to add, wait for its next ` +
              `escalation.`,
          };
        }
        if (reason === "withdrawn") {
          return {
            ok: false,
            reason: "run_not_in_flight",
            message:
              `The ask that raised question ${qid} is no longer in flight (the run was ` +
              `cancelled, or that ask already failed), so nobody is waiting for this answer. ` +
              `Use GetWorkflowRun to see the run's current state before deciding what to do next.`,
          };
        }
        return {
          ok: false,
          reason: "unknown_question",
          message:
            `Unknown question id ${qid}. It may be misspelled, or it may come from a process ` +
            `that is gone: parked questions are not persisted, so they vanish on restart (a ` +
            `resume makes the subagent ask again under a new id). Use GetWorkflowRun to read ` +
            `the run's pendingQuestions for the ids that are actually awaiting an answer.`,
        };
      }
      retire(qid, "resolved");
// settle is called after exit: it will synchronously unwind the deferred on that side of the actor, and any re-entry on that chain
// (For example, if there is another escalate in the same round), you must see a table that does not contain this qid.
      entry.settle(answer);
      return { ok: true, qid };
    },

    pendingFor(runId) {
      const out: DynamicWorkflowRunPendingQuestion[] = [];
      for (const entry of parked.values()) {
        if (entry.runId !== runId) continue;
        out.push({
          qid: entry.qid,
          actor: entry.actor,
          ...(entry.actorName === undefined ? {} : { actorName: entry.actorName }),
          question: entry.question,
          ...(entry.context === undefined ? {} : { context: entry.context }),
          askedAt: entry.askedAt,
        });
      }
      return out;
    },
  };
}
