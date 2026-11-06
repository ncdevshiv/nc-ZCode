// ============================================================
// CreateWorkflow Tool - typecheck a dynamic-workflow script and start a background run
// ============================================================

import { z } from "zod";
import { toToolJsonSchema } from "./json-schema.js";
import { SAVED_WORKFLOW_MAX_NAME_CHARS, SavedWorkflowScopeSchema } from "./saved-workflow.js";

export const CREATE_WORKFLOW_TOOL_NAME = "CreateWorkflow";

/** Violation description for "exactly one executable source". It is written as a constant because the model is its only reader, and the three paths are in the same sentence. */
export const CREATE_WORKFLOW_SOURCE_ERROR =
  "Provide exactly one workflow source: `script` for a one-off script written inline, `saved` to run a workflow saved in this project, or `path` for a script file on disk (the file a previous result named). Passing more than one, or none, is ambiguous.";

/** `args` is a violation specification that belongs only to `path` sources (`saved` has its own `saved.args`, which is not declared in inline scripts). */
export const CREATE_WORKFLOW_ARGS_WITHOUT_PATH_ERROR =
  "`args` belongs to the `path` source: it carries values for the arguments a script file declares in its `/* zcode-workflow` block. For a saved workflow pass `saved.args`; an inline `script` declares no arguments, so it takes none.";

/**
* `saved` source. The model fills in `name`, optional `args` and `scope` (for disambiguation); `path` is `resolveInput`
* The fact of **backfill** after parsing, `scope` also becomes the hit one after normalization, so both are optional.
 *
* Normalized input is therefore a valid execution state of a `script` and `saved` **simultaneously present** - this is exactly what XOR cannot be written in
* Reason on zod (see {@link CreateWorkflowInputSchema}).
 */
export const CreateWorkflowSavedSourceSchema = z
  .object({
    name: z
      .string()
      .min(1)
      .max(SAVED_WORKFLOW_MAX_NAME_CHARS)
      .describe("Name of a workflow saved in this project or globally (see ListSavedWorkflows)."),
    args: z
      .record(z.unknown())
      .optional()
      .describe(
        "Values for the arguments the saved workflow declares. Unknown keys and type mismatches are rejected before anything runs.",
      ),
/** Parsing backfill: Save the location of the file. The model is not filled in. */
    path: z.string().min(1).optional(),
    /**
* Disambiguation: From which file to get this workflow. By default, the existing search order is followed (when the same name is used, the project file covers the global file).
* After normalization, `scope` becomes the **hit** one, see create-workflow-source.ts.
     */
    scope: SavedWorkflowScopeSchema.optional().describe(
      "Which archive to take the workflow from. Omit to use the normal lookup order (a project workflow hides a global one with the same name).",
    ),
  })
  .strict();

export type CreateWorkflowSavedSource = z.infer<typeof CreateWorkflowSavedSourceSchema>;

/**
* Runtime `saved` block: those fields in the model + `draft`.
 *
* `draft` is the **drop point** that is backfilled after `resolveInput` writes the working copy: the saved definition will never be changed because of a run, it is this copy that needs to be changed in the model. it is related to
* `AmendWorkflow.predecessor` same posture - the fact is calculated by the tool, the model's JSON schema does not list it, so the model
* Don't think you should fill in a path. When writing is impossible the field is entirely absent (the draft is a best effort).
 */
export const CreateWorkflowResolvedSavedSourceSchema = CreateWorkflowSavedSourceSchema.extend({
  draft: z.string().min(1).optional(),
}).strict();

export type CreateWorkflowResolvedSavedSource = z.infer<
  typeof CreateWorkflowResolvedSavedSourceSchema
>;

/**
* There are three sources of executable bodies, and there is exactly one - but this XOR is **deliberately not written on the schema**.
 *
* The reason is normalization: after `resolveInput` parses the saved script (or `path` file), the input also contains
* `script` (the bytes to run) vs. `saved` / `path` (its ins and outs). A superRefine will blow up in two places
* This legal shape - the handler's own `parse`, and the call-runner's secondary verification after the hook rewrites the input - and
* **Only explodes on non-inline paths, which is the kind of error that goes online without being detected. XOR is therefore represented by `entry.validateInput` in
* **Model input parameters** are forced, and that is the only place where it is true. JSON schema cannot express refinement originally.
* The model side is guided by field descriptions.
 */
const CreateWorkflowModelInputSchema = z
  .object({
    name: z
      .string()
      .min(1)
      .optional()
      .describe(
        "Short display label for this run, in the user's language (\"PR review\", \"code review\"). Always pass it for an inline script: it labels the run everywhere and names its draft file under .zcode/workflow-drafts/. Defaults to the saved workflow's name when running a saved workflow.",
      ),
    script: z
      .string()
      .optional()
      .describe(
        "Full TypeScript workflow script written against the dynamic-workflow facade, inline. Provide exactly one of `script`, `saved` and `path`. An inline script is saved to a file for you, and the result names it: revise it with `path`, not by pasting the script again.",
      ),
    saved: CreateWorkflowSavedSourceSchema.optional().describe(
      "Run a workflow saved in this project or globally instead of an inline script. Provide exactly one of `script`, `saved` and `path`.",
    ),
    /**
* The third source:
* A script file on disk. It's a **return** to an inline commit - the tool writes a draft, the results name that file, and the next time the model
* Change only one line and return the same path. Files with `/* zcode-workflow` blocks are parsed as saved (blocks stripped,
* The text is used as a script, and `args` is verified according to the declaration in the block).
     */
    path: z
      .string()
      .min(1)
      .optional()
      .describe(
        "A script file on disk, relative to the working directory or absolute — normally the file a previous CreateWorkflow/AmendWorkflow result named. Provide exactly one of `script`, `saved` and `path`. Prefer this over pasting a revised script: edit the file and pass its path.",
      ),
    /**
* Actual parameters declared in the `path` file. `saved` has its own `saved.args` (the same set of validation rules), which is not declared in the inline script.
* So this field advances and retreats at the same time as `path` - `validateInput` pins this on the model input parameter.
     */
    args: z
      .record(z.unknown())
      .optional()
      .describe(
        "Values for the arguments a `path` file declares in its `/* zcode-workflow` block. Only with `path`; with `saved` use `saved.args`. Unknown keys, missing required values and type mismatches are rejected before anything runs.",
      ),
    /**
* run's own concurrency upper bound. Only lower, not raise:
* `resolveInput` is clamped to `[1, ceiling]`, and what the confirmation window and handler see is the value that will take effect. Absence is the ceiling.
* Only set when requested by the user - provider current limit is adaptive at runtime, and the model should not use it as insurance.
     */
    max_concurrency: z
      .number()
      .int()
      .positive()
      .optional()
      .describe(
        'Upper bound on how many subagents work at the same time in this run. Set it ONLY when the user asks to limit parallelism ("at most 3 at a time", "don\'t run so many at once"). Never set it on your own initiative and never in response to provider rate limits or errors — the runtime already adapts to those. A value above what this machine allows is lowered to that maximum. Omit for the default.',
      ),
    /**
* On which model this run subagent runs. and
* `max_concurrency` homogeneous: The model face receives a loose string, `resolveInput` is resolved into by the model directory port
* Standard form `providerId/modelId[$reasoningLevel]` - what the confirmation window and handler read is the value that will take effect.
* If it cannot be solved, it will be returned as a business failure before the window is opened. The master agent itself remains on the session model; absent, so are the subagents.
     */
    subagent_model: z
      .string()
      .trim()
      .min(1)
      .optional()
      .describe(
        'Model for the workflow\'s subagents, as `providerId/modelId` or a bare model id (optionally `$reasoningLevel`). Set it ONLY when the user asks for the subagents to run on a specific model ("run the subagents on GLM-5.3-Flash"). Pass the name the user used; if the tool answers that it cannot resolve it, pick from the listed ids or call ListModels. The main agent (you) keeps the session model regardless. Omit to run subagents on the session model.',
      ),
// Amend continuation is not here: it is the work of `AmendWorkflow` (amend-workflow.ts). `.strict()` makes the old way of writing `resume_from` a visible schema error,
// Instead of being silently ignored and then turned into a full price rerun.
  })
  .strict();

/**
* Runtime input parameters: those fields in the model + the fact of `resolveInput` backfill.
 *
* Backfill items are in the same pose as `AmendWorkflow.predecessor` - the model's JSON schema does not list them because they are not
* Fillable parameters are analytical results; if the model is filled in hard, normalization will be overridden unconditionally.
 */
export const CreateWorkflowInputSchema = CreateWorkflowModelInputSchema.extend({
  saved: CreateWorkflowResolvedSavedSourceSchema.optional(),
  /**
* Text line → file line offset (non-zero if `path` / `saved` files have metadata blocks). When the diagnosis is reported by file line
* Add this so that line numbers can be glued directly into an `Edit` of the file.
   */
  script_line_offset: z.number().int().nonnegative().optional(),
}).strict();

export type CreateWorkflowInput = z.infer<typeof CreateWorkflowInputSchema>;

/** The JSON schema handed to the model: does not contain `script_line_offset`, and `saved` does not contain `draft`. */
export const CreateWorkflowInputJsonSchema = toToolJsonSchema(CreateWorkflowModelInputSchema);

export const CreateWorkflowDiagnosticSchema = z
  .object({
    code: z.number(),
    column: z.number(),
    line: z.number(),
    message: z.string(),
  })
  .strict();

export type CreateWorkflowDiagnostic = z.infer<typeof CreateWorkflowDiagnosticSchema>;

// CreateWorkflow display is a bounded projection independent of the model text: diagnostics may be many and must be bounded at protocol boundaries,
// Avoid type checking results from expanding continuous/replayable messages into unbounded payloads.
// The diagnostic display entry shape is used by both create_workflow and eval_workflow_snippet displays at the same time
// payload reuse (same TS diagnostic shape, same limit length), so defined here instead of tool-result-metadata.ts
// ——The latter needs to back-reference the schema in this file, and it will form a loop if placed there.
export const CREATE_WORKFLOW_DISPLAY_MAX_DIAGNOSTICS = 100;
export const CREATE_WORKFLOW_DISPLAY_MAX_MESSAGE_CHARS = 2_048;

export const createWorkflowToolResultDisplayDiagnosticSchema = z
  .object({
    line: z.number().int().nonnegative(),
    column: z.number().int().nonnegative(),
    code: z.number().int().nonnegative(),
    message: z.string().min(1).max(CREATE_WORKFLOW_DISPLAY_MAX_MESSAGE_CHARS),
  })
  .strict();

// The Causality graph is a bounded projection of the display channel, but is bounded directly at the tool output boundary: persistent tool
// output and real-time display payload share the same contract to prevent the two from evolving different truncation semantics.
// The payload only contains the fields that the GUI actually reads: the side is
// `{from, to, back?}` A shape, region / certainty / edge type are left in the analyzer. second floor
// Is subagent-oriented: participant card + handover edge at each stage;
// The step edge is therefore no longer loaded (no readers), and the step / lane is left as a key for the run state and inspector.
export const CREATE_WORKFLOW_GRAPH_MAX_STEPS = 64;
export const CREATE_WORKFLOW_GRAPH_MAX_LANES = 32;
export const CREATE_WORKFLOW_GRAPH_MAX_PARTICIPANTS = 64;
export const CREATE_WORKFLOW_GRAPH_MAX_HANDOFFS = 256;
export const CREATE_WORKFLOW_GRAPH_MAX_HANDOFF_TYPES = 8;
export const CREATE_WORKFLOW_GRAPH_MAX_PHASES = 32;
export const CREATE_WORKFLOW_GRAPH_MAX_PHASE_EDGES = 128;
export const CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS = 64;
export const CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS = 128;

export const CREATE_WORKFLOW_STEP_KINDS = ["ask", "world-read"] as const;

// The name is only the part that is statically available when it is formed at runtime (`` agent(`researcher${i + 1}`) ``): before the first hole
// The literal (head) and the literal after the last hole (tail). At least one of the two is present - if the analyzer can't get it, the whole
// If the field is absent, the empty pattern will not be sent. **Move data only**: The ellipsis that renders it as "researcher..." is a decision of the rendering layer.
export const CreateWorkflowNamePatternSchema = z
  .object({
    head: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS).optional(),
    tail: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS).optional(),
  })
  .strict();

export type CreateWorkflowNamePattern = z.infer<typeof CreateWorkflowNamePatternSchema>;

// A step = a facade operation to wait for (ask/files.*). lane is the actor executing it (or
// workspace / unknown); lanes only appears when the receiver is may-set. At this time, the step has been expanded into candidate lanes.
// One copy per lane (with source each). repeat distinguishes two multiplicity cues: stack (instance coexistence, drawing stacked cards) and
// serial (instance succession, already expressed by closed-loop arrows).
// certainty / region does not load: the GUI does not draw them.
export const CreateWorkflowStepSchema = z
  .object({
    id: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    kind: z.enum(CREATE_WORKFLOW_STEP_KINDS),
    label: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS),
/** `label` only gets the static shape of that name when the string is inline (inline `agent()` receiver). */
    labelPattern: CreateWorkflowNamePatternSchema.optional(),
    line: z.number().int().positive().optional(),
    column: z.number().int().positive().optional(),
    lane: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    lanes: z
      .array(z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS))
      .max(CREATE_WORKFLOW_GRAPH_MAX_LANES)
      .optional(),
    /**
* The site id to expand from, only appears on copies expanded by may-set lanes (the copy id looks like `ask#2~actor#1`).
* It is an associated key that is superimposed in real time: the runtime instance reports the site id, so the card with source
* `(node.siteId === source, node.actorSiteId === lane)` Receive status. Deliberately not involved in referential integrity
* Convergence - it points to the site that was replaced by the copy, and there is no such node in the graph.
     */
    source: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS).optional(),
    /**
* The author uses `phase("…")` to mark the phased-in phase (`phase#2`, or the reserved `unphased`). with pictures
* `phases` / `phaseEdges` / `exits` advance and retreat together: all four are present (at this time **each** step has one,
* division is all), or all is absent (zero mark script).
     */
    phase: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS).optional(),
    repeat: z.enum(["stack", "serial"]).optional(),
  })
  .strict();

export type CreateWorkflowStep = z.infer<typeof CreateWorkflowStepSchema>;

// Lane = one actor (plus one workspace lane). Multiplicity no longer hangs in the driveway (it used to be families →
// nesting): the lane is no longer rendered, the number of family members is expressed by the participant's `member` / `many`.
export const CreateWorkflowLaneSchema = z
  .object({
    id: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    name: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS).optional(),
/** The static shape when `name` is absent and the first parameter of `agent()` is a template string with holes; mutually exclusive with `name`. */
    namePattern: CreateWorkflowNamePatternSchema.optional(),
    line: z.number().int().positive().optional(),
    column: z.number().int().positive().optional(),
  })
  .strict();

export type CreateWorkflowLane = z.infer<typeof CreateWorkflowLaneSchema>;

// Edges only mean one thing: runs after. The phase edge and the transition edge have the same shape. `back` marks the loop back edge - drawing method and others
// The edges are exactly the same, not labeled, only the layout ranking (the back edge does not participate in the column order) and the frame header cycle count to read it. Analyzer's
// kind / certainty / exact does not enter the load; both edges have undergone unified transitive reduction.
export const CreateWorkflowEdgeSchema = z
  .object({
    from: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    to: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    back: z.literal(true).optional(),
  })
  .strict();

export type CreateWorkflowEdge = z.infer<typeof CreateWorkflowEdgeSchema>;

// Participant = a card on the second level of the board: the subagent (or workspace /
// not parsed). When the fan-out family is expanded according to the literal cardinality, there is one piece for each member (`member`), and when the cardinality is unknown, there is one piece of `many`
//The card represents all members. `steps` is its step in this phase - the run state is aggregated from it and the inspector is listed from ask.
//The order of the array is the handover order (determined by the analyzer): the folded surface is from top to bottom, the expanded surface is from left to right, and the first one is the starter.
export const CreateWorkflowParticipantSchema = z
  .object({
/** `${phase}:${lane}`, family member `${phase}:${lane}[${index}]`. */
    id: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
/** Phase id; when the script has no phase vocabulary, it is always `unphased` (in this case `phases` is absent). */
    phase: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    lane: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    steps: z
      .array(z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS))
      .min(1)
      .max(CREATE_WORKFLOW_GRAPH_MAX_STEPS),
    member: z
      .object({
        index: z.number().int().nonnegative(),
        of: z.number().int().positive(),
      })
      .strict()
      .optional(),
    many: z.literal(true).optional(),
  })
  .strict();

export type CreateWorkflowParticipant = z.infer<typeof CreateWorkflowParticipantSchema>;

// Handover = runs after between participants (happens-before after reduction). `types` spans this edge
// Product type (data side of site map), only enter the viewer, no arrow.
export const CreateWorkflowHandoffSchema = CreateWorkflowEdgeSchema.extend({
  types: z
    .array(z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS))
    .min(1)
    .max(CREATE_WORKFLOW_GRAPH_MAX_HANDOFF_TYPES)
    .optional(),
}).strict();

export type CreateWorkflowHandoff = z.infer<typeof CreateWorkflowHandoffSchema>;

// Phase = a set of steps marked by the author with `phase("...")`. The name is the key (two tags with the same name are the same stage),
// So `name` is the author's original word; the synthesis stage is `unphased` **No name**, the display name is localized by UI
// (Same pattern as the workspace/unknown lane). loc is the position of the first mark.
export const CreateWorkflowPhaseSchema = z
  .object({
    id: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS),
    name: z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_NAME_CHARS).optional(),
    line: z.number().int().positive().optional(),
    column: z.number().int().positive().optional(),
    /**
* Other stages that are still running when entering this stage: the strands they fan-out have not yet joined. stage sequence,
* The listed stage id, excluding itself, is empty and the entire field is absent (advance and retreat with the vocabulary).
     *
* It is a **node fact** not an edge - control is not transferred from those stages, both sides are present at the same time, so it is neither
* Entering `phaseEdges` does not participate in the reduction of edges (reduction will treat it as runs after and cut off the real edges).
* The reader is the timeline: the adjacent stages strung together by it are folded into a bifurcated "belt", and a branch track is formed on the main line;
* The sidebar mini track draws double line segments accordingly.
     */
    alongside: z
      .array(z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS))
      .min(1)
      .max(CREATE_WORKFLOW_GRAPH_MAX_PHASES)
      .optional(),
  })
  .strict();

export type CreateWorkflowPhase = z.infer<typeof CreateWorkflowPhaseSchema>;

// The name follows history (it once only housed cause and effect diagrams). Three layers: the step layer is the site (the key of the running state, no longer drawn); the participant layer
// It is the card and handover of each stage (the causal fact is quotient according to the card); the stage layer is the control flow fact (the stage quotient of the control flow graph). bridge between floors
// Is `Participant.phase` / `Participant.steps`, `Step.phase` and `exits`.
export const CreateWorkflowCausalityGraphSchema = z
  .object({
    steps: z.array(CreateWorkflowStepSchema).max(CREATE_WORKFLOW_GRAPH_MAX_STEPS),
    lanes: z.array(CreateWorkflowLaneSchema).max(CREATE_WORKFLOW_GRAPH_MAX_LANES),
    participants: z
      .array(CreateWorkflowParticipantSchema)
      .max(CREATE_WORKFLOW_GRAPH_MAX_PARTICIPANTS),
    handoffs: z.array(CreateWorkflowHandoffSchema).max(CREATE_WORKFLOW_GRAPH_MAX_HANDOFFS),
    /**
* Phase glossary, with `phaseEdges`, `exits`, `Step.phase` **All or nothing**: zero-tagged script all four
* If absent, the view switching condition of the UI is "whether the vocabulary is present or not". Also absent when out of bounds (+ `truncated`)
* - A stage diagram cut in half will lie. A zero-member stage (marker only, no step) is also in the table: control flow passes through it.
     */
    phases: z.array(CreateWorkflowPhaseSchema).max(CREATE_WORKFLOW_GRAPH_MAX_PHASES).optional(),
    phaseEdges: z
      .array(CreateWorkflowEdgeSchema)
      .max(CREATE_WORKFLOW_GRAPH_MAX_PHASE_EDGES)
      .optional(),
    /**
* Control flow can be completed normally in subsequent stages (the source of the edge pointing to the sink terminal in the control flow graph stage quotient), stage table order.
* The "Phase → Return" arrow in the stage view reads it so that every arrow on that screen is a control flow. The group can be empty
* (The script does not have a normal completion path), and cannot appear alone outside the group.
     */
    exits: z
      .array(z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS))
      .max(CREATE_WORKFLOW_GRAPH_MAX_PHASES)
      .optional(),
/** Which steps provide the return items (data facts, used for drill-down); default when the script does not return. */
    sink: z
      .array(z.string().min(1).max(CREATE_WORKFLOW_GRAPH_MAX_ID_CHARS))
      .max(CREATE_WORKFLOW_GRAPH_MAX_STEPS)
      .optional(),
    truncated: z.boolean().optional(),
  })
  .strict();

export type CreateWorkflowCausalityGraph = z.infer<typeof CreateWorkflowCausalityGraphSchema>;

/**
* Declaration stage table of run: **famous** in the cause and effect diagram
* Stages, in declaration order. The running status only knows the stage that has been entered. If you want to draw "What other stations are ahead" on the sidebar mini-track, you must add this table when submitting.
* Handed to the engine as `run-launched`. Two submission paths (`CreateWorkflow` tool, hub direct startup) share this function.
* So the same script draws the same track on both roads. No diagram / No phase glossary / Only synthetic `unphased` → `undefined`
* (The entire field is absent, not an empty array: the UI draws a hidden site based on the absence).
 */
export function createWorkflowPhaseNames(
  graph: Pick<CreateWorkflowCausalityGraph, "phases"> | undefined,
): string[] | undefined {
  return namedPhases(graph)?.map((phase) => phase.name);
}

/**
* "While running" table **Positionally aligned** with {@link createWorkflowPhaseNames}: `out[i]` is i in the declaration table
* The index of the other phase that the strand is still running when the named phase is entered (the index in the same named phase table).
 *
* The subscript space is for the **named phase**, not for the original stage table of the causal diagram: the unnamed phase (synthesized `unphased`) is skipped,
* References pointing to it, pointing to unlisted stages, or pointing to itself are discarded - getting an out-of-bounds subscript in the sidebar will cause "running at the same time"
* Connected to the wrong site. It enters `run-launched` with `DynamicWorkflowRunSubmitRequest.phaseAlongside`,
* The sidebar mini track draws two adjacent stations within the belt as double line segments.
 *
* No diagram / No phase glossary / No phase with alongside → `undefined` (the field is entirely absent, rather than a string of empty
* Array: Absence means "this track is a straight line").
 */
export function createWorkflowPhaseAlongside(
  graph: Pick<CreateWorkflowCausalityGraph, "phases"> | undefined,
): number[][] | undefined {
  const named = namedPhases(graph);
  if (named === undefined) return undefined;
  const indexOf = new Map(named.map((phase, index) => [phase.id, index]));
  let any = false;
  const out = named.map((phase, self) => {
    const indexes: number[] = [];
    for (const id of phase.alongside ?? []) {
      const index = indexOf.get(id);
      if (index === undefined || index === self || indexes.includes(index)) continue;
      indexes.push(index);
    }
    any = any || indexes.length > 0;
    return indexes;
  });
  return any ? out : undefined;
}

/** In the famous stage (declaration order, cut off to the upper bound), `name` has been narrowed. */
type CreateWorkflowNamedPhase = CreateWorkflowPhase & { name: string };

/**
* The table shared by the above two functions: the **famous** stage in the causal diagram, declaration order, end
* `CREATE_WORKFLOW_GRAPH_MAX_PHASES`. The purpose of extracting them into one function is to make them follow the same filtering path and fall into the same
* In the subscript space - `phaseAlongside[i]` must refer to the station `phaseNames[i]`.
 */
function namedPhases(
  graph: Pick<CreateWorkflowCausalityGraph, "phases"> | undefined,
): CreateWorkflowNamedPhase[] | undefined {
  if (graph?.phases === undefined) return undefined;
  const named: CreateWorkflowNamedPhase[] = [];
  for (const phase of graph.phases) {
    const name = phase.name;
    if (name === undefined) continue;
    named.push({ ...phase, name });
    if (named.length >= CREATE_WORKFLOW_GRAPH_MAX_PHASES) break;
  }
  return named.length === 0 ? undefined : named;
}

// The two new fields only appear if "a run is actually started after confirmation"; the result of diagnostic-only remains unchanged.
// Use explicit schema instead of raw entrainment: The meaning of .strict() is that the shape change must be an explicit commit.
export const CreateWorkflowOutputSchema = z
  .object({
    diagnostics: z.array(CreateWorkflowDiagnosticSchema),
    ok: z.boolean(),
    response: z.string(),
    causalityGraph: CreateWorkflowCausalityGraphSchema.optional(),
/** Appears only when background run is started; it is not a general status field, so only this literal is accepted. */
    status: z.literal("backgrounded").optional(),
/** Background task id ≡ taskId ≡ runId (cancellation and status query both use it as the key). */
    backgroundTaskId: z.string().min(1).optional(),
  })
  .strict();

export type CreateWorkflowOutput = z.infer<typeof CreateWorkflowOutputSchema>;

export const CreateWorkflowOutputJsonSchema = toToolJsonSchema(CreateWorkflowOutputSchema);
