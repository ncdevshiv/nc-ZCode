/* eslint-disable max-lines -- Cohesive contract/runtime module kept in one file; split is tracked separately. */
import { PROVIDER_MODEL_SELECTION_MIGRATION_SQL } from "./migrations/0020-provider-model-selection.js";

interface SqliteMigration {
  appVersion: string;
  id: string;
  sql: string;
}

export const SQLITE_MIGRATIONS: readonly SqliteMigration[] = [
  {
    appVersion: "0.2.0",
    id: "0001_base_session_store",
    sql: `
      create table if not exists session (
        id text primary key,
        project_id text not null,
        workspace_id text,
        parent_id text,
        slug text not null,
        directory text not null,
        path text,
        title text not null,
        version text not null,
        share_url text,
        summary_additions integer,
        summary_deletions integer,
        summary_files integer,
        summary_diffs text,
        revert text,
        permission text,
        time_created integer not null,
        time_updated integer not null,
        time_compacting integer,
        time_archived integer
      );

      create index if not exists session_project_idx on session(project_id);
      create index if not exists session_workspace_idx on session(workspace_id);
      create index if not exists session_parent_idx on session(parent_id);

      create table if not exists message (
        id text primary key,
        session_id text not null references session(id) on delete cascade,
        time_created integer not null,
        time_updated integer not null,
        data text not null
      );

      create index if not exists message_session_time_created_id_idx
        on message(session_id, time_created, id);

      create table if not exists part (
        id text primary key,
        message_id text not null references message(id) on delete cascade,
        session_id text not null,
        time_created integer not null,
        time_updated integer not null,
        data text not null
      );

      create index if not exists part_message_id_id_idx on part(message_id, id);
      create index if not exists part_session_idx on part(session_id);

      create table if not exists todo (
        session_id text not null references session(id) on delete cascade,
        content text not null,
        status text not null,
        priority text not null,
        position integer not null,
        time_created integer not null,
        time_updated integer not null,
        primary key(session_id, position)
      );

      create index if not exists todo_session_idx on todo(session_id);

      create table if not exists session_entry (
        id text primary key,
        session_id text not null references session(id) on delete cascade,
        type text not null,
        time_created integer not null,
        time_updated integer not null,
        data text not null
      );

      create index if not exists session_entry_session_idx on session_entry(session_id);
      create index if not exists session_entry_session_type_idx on session_entry(session_id, type);
      create index if not exists session_entry_time_created_idx on session_entry(time_created);

      create table if not exists permission (
        project_id text primary key,
        time_created integer not null,
        time_updated integer not null,
        data text not null
      );

      create table if not exists input_history (
        id text primary key,
        project_id text not null,
        session_id text,
        text text not null,
        kind text not null,
        time_created integer not null
      );

      create index if not exists input_history_project_time_idx
        on input_history(project_id, time_created desc, id desc);
      create index if not exists input_history_time_idx
        on input_history(time_created desc, id desc);
    `,
  },
  {
    appVersion: "0.2.0",
    id: "0002_local_setting",
    sql: `
      create table if not exists local_setting (
        scope text not null,
        scope_id text not null,
        namespace text not null,
        key text not null,
        value text not null,
        schema_version integer not null,
        time_created integer not null,
        time_updated integer not null,
        primary key(scope, scope_id, namespace, key)
      );

      create index if not exists local_setting_scope_idx
        on local_setting(scope, scope_id);

      create index if not exists local_setting_namespace_key_idx
        on local_setting(namespace, key);
    `,
  },
  {
    appVersion: "0.2.0",
    id: "0003_backfill_permission_local_setting",
    sql: `
      insert or ignore into local_setting (
        scope,
        scope_id,
        namespace,
        key,
        value,
        schema_version,
        time_created,
        time_updated
      )
      select
        'project',
        project_id,
        'permission',
        'ruleset',
        data,
        1,
        time_created,
        time_updated
      from permission
      where data is not null;
    `,
  },
  {
    appVersion: "0.7.0",
    id: "0004_session_target",
    sql: `
      create table if not exists session_target (
        session_id text primary key references session(id) on delete cascade,
        target_id text not null,
        objective text not null,
        status text not null check(status in ('active', 'paused', 'complete')),
        time_created integer not null,
        time_updated integer not null
      );
    `,
  },
  {
    appVersion: "0.7.0",
    id: "0005_session_target_accounting",
    sql: `
      create table if not exists session_target_next (
        session_id text primary key references session(id) on delete cascade,
        target_id text not null,
        objective text not null,
        status text not null check(status in ('active', 'paused', 'budget_limited', 'complete')),
        token_budget integer,
        tokens_used integer not null default 0,
        time_used_seconds integer not null default 0,
        time_created integer not null,
        time_updated integer not null
      );

      insert into session_target_next (
        session_id,
        target_id,
        objective,
        status,
        token_budget,
        tokens_used,
        time_used_seconds,
        time_created,
        time_updated
      )
      select
        session_id,
        target_id,
        objective,
        status,
        null,
        0,
        0,
        time_created,
        time_updated
      from session_target;

      drop table session_target;
      alter table session_target_next rename to session_target;
    `,
  },
  {
    appVersion: "0.11.0",
    id: "0006_input_history_attachments",
    sql: `
      alter table input_history add column attachments text;
    `,
  },
  {
    appVersion: "0.13.0",
    id: "0007_workflow_script_runtime",
    sql: `
      alter table session add column task_type text not null default 'interactive';

      create index if not exists session_task_type_idx on session(task_type);

      create table if not exists workflow_definition (
        id text primary key,
        name text not null,
        source text not null check(source in ('builtin', 'user')),
        trusted integer not null default 0 check(trusted in (0, 1)),
        enabled integer not null default 1 check(enabled in (0, 1)),
        script_path text,
        script_hash text not null,
        meta_json text not null,
        time_created integer not null,
        time_updated integer not null
      );

      create index if not exists workflow_definition_source_idx
        on workflow_definition(source, enabled);

      create table if not exists workflow_run (
        id text primary key,
        definition_id text,
        name text not null,
        kind text not null default 'script',
        parent_session_id text references session(id) on delete set null,
        cwd text not null,
        script_path text,
        script_hash text not null,
        args_json text,
        args_hash text,
        status text not null check(status in (
          'pending',
          'running',
          'paused',
          'completed',
          'failed',
          'cancelled'
        )),
        current_phase text,
        budget_total integer,
        budget_spent integer not null default 0,
        stats_json text,
        failure_json text,
        time_created integer not null,
        time_started integer,
        time_updated integer not null,
        time_completed integer
      );

      create index if not exists workflow_run_parent_session_idx
        on workflow_run(parent_session_id);
      create index if not exists workflow_run_cwd_status_idx
        on workflow_run(cwd, status, time_updated desc);
      create index if not exists workflow_run_definition_idx
        on workflow_run(definition_id);

      create table if not exists workflow_activity (
        id text primary key,
        run_id text not null references workflow_run(id) on delete cascade,
        parent_activity_id text,
        call_index integer not null,
        call_path text not null,
        attempt integer not null default 1,
        type text not null,
        phase text,
        label text,
        input_hash text not null,
        prompt text,
        opts_json text,
        status text not null check(status in (
          'queued',
          'running',
          'completed',
          'failed',
          'skipped',
          'cancelled',
          'cached',
          'lost'
        )),
        child_session_id text references session(id) on delete set null,
        result_json text,
        error_json text,
        time_created integer not null,
        time_started integer,
        time_updated integer not null,
        time_completed integer,
        unique(run_id, call_path, attempt)
      );

      create index if not exists workflow_activity_run_status_idx
        on workflow_activity(run_id, status, call_index);
      create index if not exists workflow_activity_child_session_idx
        on workflow_activity(child_session_id);

      create table if not exists workflow_event (
        id text primary key,
        run_id text not null references workflow_run(id) on delete cascade,
        sequence integer not null,
        type text not null,
        phase text,
        activity_id text references workflow_activity(id) on delete set null,
        payload_json text,
        time_created integer not null,
        unique(run_id, sequence)
      );

      create index if not exists workflow_event_run_sequence_idx
        on workflow_event(run_id, sequence);

      create table if not exists session_task_link (
        id text primary key,
        root_workflow_run_id text references workflow_run(id) on delete cascade,
        parent_link_id text references session_task_link(id) on delete cascade,
        activity_id text references workflow_activity(id) on delete set null,
        parent_session_id text references session(id) on delete set null,
        child_session_id text not null references session(id) on delete cascade,
        role text not null,
        depth integer not null default 0,
        path text not null,
        phase text,
        label text,
        agent_type text,
        model text,
        status text not null,
        time_created integer not null,
        time_updated integer not null,
        unique(child_session_id)
      );

      create index if not exists session_task_link_root_workflow_idx
        on session_task_link(root_workflow_run_id, depth, path);
      create index if not exists session_task_link_parent_idx
        on session_task_link(parent_link_id);
      create index if not exists session_task_link_activity_idx
        on session_task_link(activity_id);
    `,
  },
  {
    appVersion: "0.13.0",
    id: "0008_workflow_definition_scope",
    sql: `
      alter table workflow_definition
        add column scope text not null default 'explicit'
        check(scope in ('builtin', 'explicit', 'project', 'user'));
    `,
  },
  {
    appVersion: "0.14.0",
    id: "0009_session_title_metadata",
    sql: `
      alter table session
        add column title_source text not null default 'first_input'
        check(title_source in ('default', 'first_input', 'generated', 'custom'));

      alter table session
        add column title_message_id text;

      alter table session
        add column time_title_updated integer;
    `,
  },
  {
    appVersion: "0.15.0",
    id: "0010_usage_observability",
    sql: `
      create table if not exists model_usage (
        id text primary key,
        logical_request_id text not null,
        attempt_index integer not null default 0,
        session_id text not null references session(id) on delete cascade,
        turn_id text,
        trace_id text,
        span_id text,
        assistant_message_id text,
        parent_user_message_id text,
        query_source text not null,
        provider_id text not null,
        model_id text not null,
        variant text,
        agent text,
        mode text,
        task_type text,
        status text not null check(status in ('running', 'completed', 'error', 'cancelled')),
        started_at integer not null,
        first_token_at integer,
        completed_at integer,
        duration_ms integer,
        time_to_first_token_ms integer,
        finish_reason text,
        tool_call_count integer not null default 0,
        input_tokens integer not null default 0,
        output_tokens integer not null default 0,
        reasoning_tokens integer not null default 0,
        cache_creation_input_tokens integer not null default 0,
        cache_read_input_tokens integer not null default 0,
        provider_total_tokens integer,
        computed_total_tokens integer not null default 0,
        retry_count integer not null default 0,
        retryable integer not null default 0 check(retryable in (0, 1)),
        cancelled_by_user integer not null default 0 check(cancelled_by_user in (0, 1)),
        context_exceeded integer not null default 0 check(context_exceeded in (0, 1)),
        error_type text,
        error_code text,
        error_message text,
        raw_usage_json text,
        provider_metadata_json text
      );

      create index if not exists model_usage_started_model_idx
        on model_usage(started_at, provider_id, model_id);
      create index if not exists model_usage_session_turn_idx
        on model_usage(session_id, turn_id);
      create index if not exists model_usage_trace_idx
        on model_usage(trace_id);
      create index if not exists model_usage_query_source_idx
        on model_usage(query_source);

      create table if not exists turn_usage (
        session_id text not null references session(id) on delete cascade,
        turn_id text not null,
        trace_id text,
        user_message_id text,
        status text not null check(status in ('running', 'completed', 'error', 'cancelled')),
        started_at integer not null,
        first_model_start_at integer,
        first_token_at integer,
        completed_at integer,
        duration_ms integer,
        time_to_first_token_ms integer,
        model_request_count integer not null default 0,
        model_retry_count integer not null default 0,
        tool_call_count integer not null default 0,
        tool_error_count integer not null default 0,
        input_tokens integer not null default 0,
        output_tokens integer not null default 0,
        reasoning_tokens integer not null default 0,
        cache_creation_input_tokens integer not null default 0,
        cache_read_input_tokens integer not null default 0,
        computed_total_tokens integer not null default 0,
        retryable integer not null default 0 check(retryable in (0, 1)),
        cancelled_by_user integer not null default 0 check(cancelled_by_user in (0, 1)),
        context_exceeded integer not null default 0 check(context_exceeded in (0, 1)),
        error_type text,
        error_code text,
        primary key(session_id, turn_id)
      );

      create index if not exists turn_usage_started_idx
        on turn_usage(started_at);

      create table if not exists tool_usage (
        id text primary key,
        session_id text not null references session(id) on delete cascade,
        turn_id text,
        trace_id text,
        tool_call_id text not null,
        tool_name text not null,
        side_effect_scope text,
        read_only integer check(read_only in (0, 1)),
        destructive integer check(destructive in (0, 1)),
        approval_status text,
        status text not null check(status in ('running', 'completed', 'error', 'cancelled')),
        started_at integer not null,
        first_output_at integer,
        completed_at integer,
        duration_ms integer,
        time_to_first_output_ms integer,
        exit_code integer,
        output_bytes integer not null default 0,
        stdout_bytes integer not null default 0,
        stderr_bytes integer not null default 0,
        truncated integer not null default 0 check(truncated in (0, 1)),
        retry_count integer not null default 0,
        retryable integer not null default 0 check(retryable in (0, 1)),
        cancelled_by_user integer not null default 0 check(cancelled_by_user in (0, 1)),
        error_type text,
        error_code text,
        error_message text
      );

      create unique index if not exists tool_usage_session_tool_call_idx
        on tool_usage(session_id, tool_call_id);
      create index if not exists tool_usage_started_tool_idx
        on tool_usage(started_at, tool_name);
      create index if not exists tool_usage_session_turn_idx
        on tool_usage(session_id, turn_id);
    `,
  },
  {
    appVersion: "0.15.0",
    id: "0011_session_target_summary_title",
    sql: `
      alter table session_target add column summary_title text;
    `,
  },
  {
    appVersion: "0.15.0",
    id: "0012_session_trace_id",
    sql: `
      alter table session add column trace_id text;

      create index if not exists session_trace_idx on session(trace_id);
    `,
  },
  {
    appVersion: "0.15.0",
    id: "0013_session_target_active_run_accounting",
    sql: `
      alter table session_target add column active_input_id text;
      alter table session_target add column active_run_started_at integer;
      alter table session_target add column active_run_last_seen_at integer;
    `,
  },
  {
    appVersion: "0.15.0",
    id: "0014_message_part_sequence",
    sql: `
      alter table message add column sequence integer;
      alter table part add column sequence integer;

      with ordered_message as (
        select
          id,
          row_number() over (
            partition by session_id
            order by time_created, rowid
          ) - 1 as stable_sequence
        from message
      )
      update message
      set sequence = (
        select stable_sequence
        from ordered_message
        where ordered_message.id = message.id
      )
      where sequence is null;

      with ordered_part as (
        select
          id,
          row_number() over (
            partition by message_id
            order by time_created, rowid
          ) - 1 as stable_sequence
        from part
      )
      update part
      set sequence = (
        select stable_sequence
        from ordered_part
        where ordered_part.id = part.id
      )
      where sequence is null;

      create index if not exists message_session_sequence_idx
        on message(session_id, sequence, time_created, id);

      create index if not exists part_message_sequence_idx
        on part(message_id, sequence, time_created, id);

      create index if not exists part_session_message_sequence_idx
        on part(session_id, message_id, sequence);
    `,
  },
  {
    appVersion: "0.15.2",
    id: "0015_message_part_sequence_backfill_and_guard",
// Background: NULL sequence continues to appear after 0014 backfill
// (Local observation message 1,690 / part 5,933 lines, spanning 331 sessions), the main suspect is an old version of the binary
//Write to the same DB concurrently (its INSERT does not contain the sequence column). This migration does two things:
// 1. Incremental backfill: only NULL rows are added, the sequence number starts from the existing max(sequence)+1 of each scope, press
// time_created/rowid row - exactly the same as the read path fallback (sequence is null after non-null)
// Consistent, the order of hydrate before and after backfill remains unchanged. The row_number-1 writing method of 0014 cannot be reused:
// It is numbered according to the full amount of rows. When the data is mixed, it will collide with the existing sequence and rearrange NULL rows to the front.
// 2. AFTER INSERT trigger: when the old binary is written into the NULL sequence, the end of the current scope queue is automatically added.
// Prevent new NULLs from being generated from the source; the new code path sequence is always non-empty and the trigger does not take effect.
    sql: `
      with session_max as (
        select session_id, coalesce(max(sequence), -1) as max_sequence
        from message
        group by session_id
      ),
      ordered_null_message as (
        select
          m.id as id,
          sm.max_sequence + row_number() over (
            partition by m.session_id
            order by m.time_created, m.rowid
          ) as stable_sequence
        from message m
        join session_max sm on sm.session_id = m.session_id
        where m.sequence is null
      )
      update message
      set sequence = (
        select stable_sequence
        from ordered_null_message
        where ordered_null_message.id = message.id
      )
      where sequence is null;

      with message_max as (
        select message_id, coalesce(max(sequence), -1) as max_sequence
        from part
        group by message_id
      ),
      ordered_null_part as (
        select
          p.id as id,
          mm.max_sequence + row_number() over (
            partition by p.message_id
            order by p.time_created, p.rowid
          ) as stable_sequence
        from part p
        join message_max mm on mm.message_id = p.message_id
        where p.sequence is null
      )
      update part
      set sequence = (
        select stable_sequence
        from ordered_null_part
        where ordered_null_part.id = part.id
      )
      where sequence is null;

      create trigger if not exists message_sequence_autofill
      after insert on message
      when new.sequence is null
      begin
        update message
        set sequence = (
          select coalesce(max(sequence), -1) + 1
          from message
          where session_id = new.session_id
        )
        where id = new.id;
      end;

      create trigger if not exists part_sequence_autofill
      after insert on part
      when new.sequence is null
      begin
        update part
        set sequence = (
          select coalesce(max(sequence), -1) + 1
          from part
          where message_id = new.message_id
        )
        where id = new.id;
      end;
    `,
  },
  {
    appVersion: "0.15.2",
    id: "0016_session_input_ledger",
// session_input ledger: input durable life cycle
// admitted -> promoted / canceled / discarded. Queue/Wake Existence If only in
// Process memory (the event log is also in memory), it will be lost silently if it crashes; the ledger will make "queue disappear but not enter"
// history" cannot occur silently and provides durable idempotence for the input class command.
    sql: `
      create table if not exists session_input (
        id text primary key,
        session_id text not null references session(id) on delete cascade,
        kind text not null,
        delivery text not null check(delivery in ('guide', 'queue')),
        payload text not null,
        admitted_sequence integer not null,
        promoted_sequence integer,
        promoted_message_id text,
        status text not null check(status in ('admitted', 'promoted', 'cancelled', 'discarded')),
        status_reason text,
        time_created integer not null,
        time_updated integer not null
      );

      create index if not exists session_input_session_admitted_idx
        on session_input(session_id, admitted_sequence);
      create index if not exists session_input_session_status_idx
        on session_input(session_id, status);
    `,
  },
  {
    appVersion: "0.15.2",
    id: "0017_session_input_start_now_delivery",
// All input commands are placed in durable admission before execution, and startNow also needs to be independent
// delivery cannot be disguised as queue. SQLite cannot modify CHECK in place, the table must be rebuilt and the ledger must be preserved.
    sql: `
      alter table session_input rename to session_input_before_start_now;

      create table session_input (
        id text primary key,
        session_id text not null references session(id) on delete cascade,
        kind text not null,
        delivery text not null check(delivery in ('startNow', 'guide', 'queue')),
        payload text not null,
        admitted_sequence integer not null,
        promoted_sequence integer,
        promoted_message_id text,
        status text not null check(status in ('admitted', 'promoted', 'cancelled', 'discarded')),
        status_reason text,
        time_created integer not null,
        time_updated integer not null
      );

      insert into session_input (
        id, session_id, kind, delivery, payload, admitted_sequence,
        promoted_sequence, promoted_message_id, status, status_reason,
        time_created, time_updated
      )
      select
        id, session_id, kind, delivery, payload, admitted_sequence,
        promoted_sequence, promoted_message_id, status, status_reason,
        time_created, time_updated
      from session_input_before_start_now;

      drop table session_input_before_start_now;

      create index session_input_session_admitted_idx
        on session_input(session_id, admitted_sequence);
      create index session_input_session_status_idx
        on session_input(session_id, status);
    `,
  },
  {
    appVersion: "0.15.2",
    id: "0018_session_input_failed_status",
// After the fork bundle is submitted, the child runtime may still fail to start synchronously. The input has been parented
// accepted fact is accepted and cannot be disguised as canceled/discarded; add durable failed final state and pass
// Rebuild CHECK to ensure that the old library can be written after it is upgraded, and restarting will not consume or rewrite it again.
    sql: `
      alter table session_input rename to session_input_before_failed_status;

      create table session_input (
        id text primary key,
        session_id text not null references session(id) on delete cascade,
        kind text not null,
        delivery text not null check(delivery in ('startNow', 'guide', 'queue')),
        payload text not null,
        admitted_sequence integer not null,
        promoted_sequence integer,
        promoted_message_id text,
        status text not null check(status in ('admitted', 'promoted', 'cancelled', 'discarded', 'failed')),
        status_reason text,
        time_created integer not null,
        time_updated integer not null
      );

      insert into session_input (
        id, session_id, kind, delivery, payload, admitted_sequence,
        promoted_sequence, promoted_message_id, status, status_reason,
        time_created, time_updated
      )
      select
        id, session_id, kind, delivery, payload, admitted_sequence,
        promoted_sequence, promoted_message_id, status, status_reason,
        time_created, time_updated
      from session_input_before_failed_status;

      drop table session_input_before_failed_status;

      create index session_input_session_admitted_idx
        on session_input(session_id, admitted_sequence);
      create index session_input_session_status_idx
        on session_input(session_id, status);
    `,
  },
  {
    appVersion: "0.16.5",
    id: "0019_dwf_journal",
//Durable journal for dynamic-workflow execution engine.
// The legacy workflow_* table is just a template, not a home: dwf_* is self-contained and independent of the existing workflow mechanism.
    //
// This article is a single baseline** compiled from the twelve migrations in the development period 0019-0030 before beta: four tables are built at once, and the shape is
// Final state after 0030. The intermediate state (three times of rebuilding the entire table to relax CHECK, deleting columns and renaming 0028) only exists in
// In the internal preview library, the convergence method is to delete the four dwf_* tables and clear the dwf accounting rows in schema_migration.
// The next startup will be rebuilt by this article. This article after beta
// Can no longer be changed: runner records according to checksum, historical migration can only be appended.
    //
// Just to occupy the slot 0019, let
// Staging subsequent migrations will be numbered from 0020, and the ledger will not conflict with the number when the function branch is merged. The four tables are harmless until the function is implemented.
    //
// Several deliberate designs:
// 1) parent_session_id / session_id is pure text, without FOREIGN KEY - the sub-agent session runs in memory
// There is no session line in the event store, and the runner has pragma foreign_keys = on, so FK is added.
// Will keep legitimate journal records out. No tables other than dwf_* are referenced, nor are they referenced.
// 2) dwf_run has no node upper limit/token budget column: run level
//Token usage is only for observation, that is, spent_tokens.
// 3) unique(run_id, actor_id, actor_seq) of dwf_node cannot be implemented: putNode is admission → settlement → statistics
// Backfilled upsert, actor coordinates are spread over three nullable columns; actor_seq uniqueness per subagent is maintained by the engine.
// report / artifact has rows but no nodes: one write, status is always completed, and actor columns are all empty.
// 4) Nullable columns are always "NULL means absent": result_json / name / tool_call_id / args_json /
// resumed_from / resolved_model / message_boundary / artifact_id / input_json are all decoded into
// Absent keys (args_json resolves to `{}`), no dummy values ​​are stored.
// 5) The index is the query shape (if the column order is reversed, the entire table can only be scanned):
// - dwf_run_cwd_idx: Enumerate historical run by cwd, "cwd equivalent + time_updated reverse order + limit".
// - dwf_node_artifact_idx: The product row of this run and the report row with label obtained by id.
// - dwf_event_artifact_idx: Kanban data is paged according to journal sequence, and the data source is dwf_event instead of
// dwf_node; expression index (SQLite ≥ 3.9) so that it does not have to scan the entire journal. The third column sequence is not
// Decoration - without it the planner would rather use the automatic indexing of unique(run_id, sequence) and then filter the products row by row.
    sql: `
      create table if not exists dwf_run (
        id text primary key,
        parent_session_id text,
        cwd text,
        name text,
        script_text text,
        script_hash text,
        args_json text,
        tool_call_id text,
        resumed_from text,
        caps_max_concurrency integer not null,
        spent_tokens integer not null default 0,
        status text not null check(status in (
          'pending',
          'running',
          'completed',
          'failed',
          'cancelled'
        )),
        result_json text,
        failure_json text,
        time_created integer not null,
        time_updated integer not null
      );

      create index if not exists dwf_run_cwd_idx on dwf_run(cwd, time_updated);

      create table if not exists dwf_actor (
        id integer primary key autoincrement,
        run_id text not null references dwf_run(id) on delete cascade,
        site_id text not null,
        ordinal integer not null,
        name text,
        persona_json text,
        resolved_model text,
        session_id text,
        time_created integer not null,
        time_updated integer not null,
        unique(run_id, site_id, ordinal)
      );

      create index if not exists dwf_actor_run_idx on dwf_actor(run_id);

      create table if not exists dwf_node (
        id integer primary key autoincrement,
        run_id text not null references dwf_run(id) on delete cascade,
        site_id text not null,
        ordinal integer not null,
        kind text not null check(kind in ('ask', 'world-read', 'world-run', 'report', 'artifact')),
        actor_site_id text,
        actor_ordinal integer,
        actor_seq integer,
        input_hash text not null,
        input_json text,
        status text not null check(status in ('running', 'completed', 'failed')),
        result_json text,
        error_json text,
        stats_json text,
        message_boundary integer,
        artifact_id text,
        time_created integer not null,
        time_updated integer not null,
        unique(run_id, site_id, ordinal)
      );

      create index if not exists dwf_node_run_idx on dwf_node(run_id);
      create index if not exists dwf_node_artifact_idx on dwf_node(run_id, artifact_id);

      create table if not exists dwf_event (
        id integer primary key autoincrement,
        run_id text not null references dwf_run(id) on delete cascade,
        sequence integer not null,
        type text not null,
        payload_json text not null,
        time_created integer not null,
        unique(run_id, sequence)
      );

      create index if not exists dwf_event_artifact_idx
        on dwf_event(run_id, json_extract(payload_json, '$.artifactId'), sequence);
    `,
  },
  {
    appVersion: "0.16.5",
    id: "0020_provider_model_selection",
    sql: PROVIDER_MODEL_SELECTION_MIGRATION_SQL,
  },
  {
    appVersion: "0.16.5",
    id: "0021_official_glm_selection",
    sql: OFFICIAL_GLM_SELECTION_MIGRATION_SQL,
  },
  {
    appVersion: "0.16.5",
    id: "0022_backfilled_session_reasoning",
    sql: BACKFILLED_SESSION_REASONING_MIGRATION_SQL,
  },
];
import { OFFICIAL_GLM_SELECTION_MIGRATION_SQL } from "./migrations/0021-official-glm-selection.js";
import { BACKFILLED_SESSION_REASONING_MIGRATION_SQL } from "./migrations/0022-backfilled-session-reasoning.js";
