--
-- PostgreSQL database dump
--

\restrict PB6S55cykNQAiWacfNKwe1QN1cX9mxCJcOQYuBtHbkExGS20vPgoJ7gUgUcisfG

-- Dumped from database version 16.15
-- Dumped by pg_dump version 16.15

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: graphile_worker; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA graphile_worker;


--
-- Name: job_spec; Type: TYPE; Schema: graphile_worker; Owner: -
--

CREATE TYPE graphile_worker.job_spec AS (
	identifier text,
	payload json,
	queue_name text,
	run_at timestamp with time zone,
	max_attempts smallint,
	job_key text,
	priority smallint,
	flags text[]
);


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: _private_jobs; Type: TABLE; Schema: graphile_worker; Owner: -
--

CREATE TABLE graphile_worker._private_jobs (
    id bigint NOT NULL,
    job_queue_id integer,
    task_id integer NOT NULL,
    payload json DEFAULT '{}'::json NOT NULL,
    priority smallint DEFAULT 0 NOT NULL,
    run_at timestamp with time zone DEFAULT now() NOT NULL,
    attempts smallint DEFAULT 0 NOT NULL,
    max_attempts smallint DEFAULT 25 NOT NULL,
    last_error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    key text,
    locked_at timestamp with time zone,
    locked_by text,
    revision integer DEFAULT 0 NOT NULL,
    flags jsonb,
    is_available boolean GENERATED ALWAYS AS (((locked_at IS NULL) AND (attempts < max_attempts))) STORED NOT NULL,
    CONSTRAINT jobs_key_check CHECK (((length(key) > 0) AND (length(key) <= 512))),
    CONSTRAINT jobs_max_attempts_check CHECK ((max_attempts >= 1))
);


--
-- Name: add_job(text, json, text, timestamp with time zone, integer, text, integer, text[], text); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.add_job(identifier text, payload json DEFAULT NULL::json, queue_name text DEFAULT NULL::text, run_at timestamp with time zone DEFAULT NULL::timestamp with time zone, max_attempts integer DEFAULT NULL::integer, job_key text DEFAULT NULL::text, priority integer DEFAULT NULL::integer, flags text[] DEFAULT NULL::text[], job_key_mode text DEFAULT 'replace'::text) RETURNS graphile_worker._private_jobs
    LANGUAGE plpgsql
    AS $$
declare
    v_job graphile_worker._private_jobs;
begin
    if (job_key is null or job_key_mode is null or job_key_mode in ('replace', 'preserve_run_at')) then
        select * into v_job
        from graphile_worker.add_jobs(
            ARRAY[(
                identifier,
                payload,
                queue_name,
                run_at,
                max_attempts::smallint,
                job_key,
                priority::smallint,
                flags
            )::graphile_worker.job_spec],
            (job_key_mode = 'preserve_run_at')
        )
        limit 1;

        if v_job.id is null then
            raise exception 'Unable to schedule job for key "%"', coalesce(job_key, '<null>') using errcode = 'GWNOR';
        end if;

        return v_job;
    elsif job_key_mode = 'unsafe_dedupe' then
        insert into graphile_worker._private_tasks as tasks (identifier)
        values (identifier)
        on conflict do nothing;

        if queue_name is not null then
            insert into graphile_worker._private_job_queues as job_queues (queue_name)
            values (queue_name)
            on conflict do nothing;
        end if;

        insert into graphile_worker._private_jobs as jobs (
            job_queue_id,
            task_id,
            payload,
            run_at,
            max_attempts,
            key,
            priority,
            flags
        )
        select
            job_queues.id,
            tasks.id,
            coalesce(payload, '{}'::json),
            coalesce(run_at, now()),
            coalesce(max_attempts::smallint, 25::smallint),
            job_key,
            coalesce(priority::smallint, 0::smallint),
            (
                select jsonb_object_agg(flag, true)
                from unnest(flags) as item(flag)
            )
        from graphile_worker._private_tasks as tasks
        left join graphile_worker._private_job_queues as job_queues
        on job_queues.queue_name = add_job.queue_name
        where tasks.identifier = add_job.identifier
        on conflict (key)
        do update set
            revision = jobs.revision + 1,
            updated_at = now()
        returning *
        into v_job;

        if v_job.revision = 0 then
            perform pg_notify('jobs:insert', '{"r":' || random()::text || ',"count":1}');
        end if;

        return v_job;
    else
        raise exception 'Invalid job_key_mode value, expected ''replace'', ''preserve_run_at'' or ''unsafe_dedupe''.' using errcode = 'GWBKM';
    end if;
end;
$$;


--
-- Name: add_jobs(graphile_worker.job_spec[], boolean); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.add_jobs(specs graphile_worker.job_spec[], job_key_preserve_run_at boolean DEFAULT false) RETURNS SETOF graphile_worker._private_jobs
    LANGUAGE plpgsql
    AS $$
declare
    locked_key text;
begin
    for locked_key in
        select distinct spec.job_key
        from unnest(specs) spec
        where spec.job_key is not null
        order by spec.job_key
    loop
        perform pg_advisory_xact_lock(hashtext(locked_key));
    end loop;

    perform 1
    from graphile_worker._private_jobs as jobs
    where jobs.key = any(
        array(
            select distinct spec.job_key
            from unnest(specs) spec
            where spec.job_key is not null
        )
    )
    for update;

    insert into graphile_worker._private_tasks as tasks (identifier)
    select distinct spec.identifier
    from unnest(specs) spec
    on conflict do nothing;

    insert into graphile_worker._private_job_queues as job_queues (queue_name)
    select distinct spec.queue_name
    from unnest(specs) spec
    where spec.queue_name is not null
    on conflict do nothing;

    update graphile_worker._private_jobs as jobs
    set
        key = null,
        attempts = jobs.max_attempts,
        updated_at = now()
    from unnest(specs) spec
    where spec.job_key is not null
    and jobs.key = spec.job_key
    and is_available is not true;

    perform pg_notify('jobs:insert', '{"r":' || random()::text || ',"count":' || array_length(specs, 1)::text || '}');

    return query insert into graphile_worker._private_jobs as jobs (
        job_queue_id,
        task_id,
        payload,
        run_at,
        max_attempts,
        key,
        priority,
        flags
    )
        select
            job_queues.id,
            tasks.id,
            coalesce(spec.payload, '{}'::json),
            coalesce(spec.run_at, now()),
            coalesce(spec.max_attempts, 25),
            spec.job_key,
            coalesce(spec.priority, 0),
            (
                select jsonb_object_agg(flag, true)
                from unnest(spec.flags) as item(flag)
            )
        from unnest(specs) spec
        inner join graphile_worker._private_tasks as tasks
        on tasks.identifier = spec.identifier
        left join graphile_worker._private_job_queues as job_queues
        on job_queues.queue_name = spec.queue_name
    on conflict (key) do update set
        job_queue_id = excluded.job_queue_id,
        task_id = excluded.task_id,
        payload =
            case
            when json_typeof(jobs.payload) = 'array' and json_typeof(excluded.payload) = 'array' then
                (jobs.payload::jsonb || excluded.payload::jsonb)::json
            else
                excluded.payload
            end,
        max_attempts = excluded.max_attempts,
        run_at = (case
            when job_key_preserve_run_at is true and jobs.attempts = 0 then jobs.run_at
            else excluded.run_at
        end),
        priority = excluded.priority,
        revision = jobs.revision + 1,
        flags = excluded.flags,
        attempts = 0,
        last_error = null,
        updated_at = now()
    where jobs.locked_at is null
    returning *;
end;
$$;


--
-- Name: complete_jobs(bigint[]); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.complete_jobs(job_ids bigint[]) RETURNS SETOF graphile_worker._private_jobs
    LANGUAGE sql
    AS $$
    delete from graphile_worker._private_jobs as jobs
        where id = any(job_ids)
        and (
            locked_at is null
        or
            locked_at < now() - interval '4 hours'
        )
        returning *;
$$;


--
-- Name: delete_stale_workers(text[]); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.delete_stale_workers(worker_ids text[]) RETURNS void
    LANGUAGE sql
    AS $$
    DELETE FROM graphile_worker._private_workers AS workers
    WHERE workers.id = ANY(worker_ids);
$$;


--
-- Name: force_unlock_workers(text[]); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.force_unlock_workers(worker_ids text[]) RETURNS void
    LANGUAGE sql
    AS $$
        update graphile_worker._private_jobs as jobs
        set locked_at = null, locked_by = null
        where locked_by = any(worker_ids);
        update graphile_worker._private_job_queues as job_queues
        set locked_at = null, locked_by = null
        where locked_by = any(worker_ids);
$$;


--
-- Name: list_orphan_locked_workers(interval); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.list_orphan_locked_workers(stale_threshold interval) RETURNS TABLE(worker_id text)
    LANGUAGE sql
    AS $$
    SELECT DISTINCT locks.locked_by AS worker_id
    FROM (
        SELECT jobs.locked_by, jobs.locked_at
        FROM graphile_worker._private_jobs AS jobs
        WHERE jobs.locked_by IS NOT NULL
        UNION ALL
        SELECT job_queues.locked_by, job_queues.locked_at
        FROM graphile_worker._private_job_queues AS job_queues
        WHERE job_queues.locked_by IS NOT NULL
    ) AS locks
    WHERE locks.locked_at < now() - stale_threshold
    AND NOT EXISTS (
        SELECT 1
        FROM graphile_worker._private_workers AS workers
        WHERE workers.id = locks.locked_by
    );
$$;


--
-- Name: list_stale_workers(interval); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.list_stale_workers(stale_threshold interval) RETURNS TABLE(worker_id text)
    LANGUAGE sql
    AS $$
    SELECT workers.id AS worker_id
    FROM graphile_worker._private_workers AS workers
    WHERE workers.last_heartbeat_at < now() - stale_threshold;
$$;


--
-- Name: permanently_fail_jobs(bigint[], text); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.permanently_fail_jobs(job_ids bigint[], error_message text DEFAULT NULL::text) RETURNS SETOF graphile_worker._private_jobs
    LANGUAGE sql
    AS $$
    UPDATE graphile_worker._private_jobs AS jobs
    SET
        last_error = COALESCE(error_message, 'Manually marked as failed'),
        attempts = max_attempts,
        updated_at = now()
    WHERE id = ANY(job_ids)
    AND (
        locked_at IS NULL
    OR
        locked_at < NOW() - interval '4 hours'
    )
    RETURNING *;
$$;


--
-- Name: recover_dead_worker_jobs(text[], interval); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.recover_dead_worker_jobs(worker_ids text[], recovery_delay interval DEFAULT '00:00:00'::interval) RETURNS integer
    LANGUAGE plpgsql
    AS $$
DECLARE
    recovered_count integer := 0;
BEGIN
    WITH j AS (
        UPDATE graphile_worker._private_jobs AS jobs
        SET
            attempts = GREATEST(0, jobs.attempts - 1),
            locked_by = NULL,
            locked_at = NULL,
            run_at = GREATEST(jobs.run_at, now() + recovery_delay),
            last_error = 'Job recovered after worker interruption',
            updated_at = now()
        WHERE jobs.locked_by = ANY(worker_ids)
        RETURNING jobs.job_queue_id
    )
    SELECT COUNT(*) INTO recovered_count FROM j;

    UPDATE graphile_worker._private_job_queues AS job_queues
    SET locked_by = NULL, locked_at = NULL
    WHERE job_queues.locked_by = ANY(worker_ids);

    RETURN recovered_count;
END;
$$;


--
-- Name: remove_job(text); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.remove_job(job_key text) RETURNS graphile_worker._private_jobs
    LANGUAGE plpgsql STRICT
    AS $$
declare
    v_job graphile_worker._private_jobs;
begin
    -- Delete job if not locked
    DELETE FROM graphile_worker._private_jobs AS jobs
    WHERE key = job_key
    AND (
        locked_at IS NULL
    OR
        locked_at < NOW() - interval '4 hours'
    )
    RETURNING * INTO v_job;
    IF NOT (v_job IS NULL) THEN
        perform pg_notify('jobs:insert', '{"r":' || random()::text || ',"count":-1}');
        RETURN v_job;
    END IF;
    -- Otherwise prevent job from retrying, and clear the key
    UPDATE graphile_worker._private_jobs AS jobs
    SET
        key = NULL,
        attempts = jobs.max_attempts,
        updated_at = now()
    WHERE key = job_key
    RETURNING * INTO v_job;
    RETURN v_job;
end;
$$;


--
-- Name: reschedule_jobs(bigint[], timestamp with time zone, integer, integer, integer); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.reschedule_jobs(job_ids bigint[], run_at timestamp with time zone DEFAULT NULL::timestamp with time zone, priority integer DEFAULT NULL::integer, attempts integer DEFAULT NULL::integer, max_attempts integer DEFAULT NULL::integer) RETURNS SETOF graphile_worker._private_jobs
    LANGUAGE sql
    AS $$
    UPDATE graphile_worker._private_jobs AS jobs
    SET
        run_at = COALESCE(reschedule_jobs.run_at, jobs.run_at),
        priority = COALESCE(reschedule_jobs.priority::smallint, jobs.priority),
        attempts = COALESCE(reschedule_jobs.attempts::smallint, jobs.attempts),
        max_attempts = COALESCE(reschedule_jobs.max_attempts::smallint, jobs.max_attempts),
        updated_at = now()
    WHERE id = ANY(job_ids)
    AND (
        locked_at IS NULL
    OR
        locked_at < NOW() - interval '4 hours'
    )
    RETURNING *;
$$;


--
-- Name: worker_deregister(text); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.worker_deregister(worker_id text) RETURNS void
    LANGUAGE sql
    AS $$
    DELETE FROM graphile_worker._private_workers
    WHERE id = worker_deregister.worker_id;
$$;


--
-- Name: worker_heartbeat(text, json); Type: FUNCTION; Schema: graphile_worker; Owner: -
--

CREATE FUNCTION graphile_worker.worker_heartbeat(worker_id text, metadata json DEFAULT NULL::json) RETURNS void
    LANGUAGE sql
    AS $$
    INSERT INTO graphile_worker._private_workers AS workers (id, last_heartbeat_at, metadata)
    VALUES (worker_heartbeat.worker_id, now(), metadata::jsonb)
    ON CONFLICT (id) DO UPDATE
    SET
        last_heartbeat_at = now(),
        metadata = COALESCE(EXCLUDED.metadata, workers.metadata);
$$;


--
-- Name: _private_job_queues; Type: TABLE; Schema: graphile_worker; Owner: -
--

CREATE TABLE graphile_worker._private_job_queues (
    id integer NOT NULL,
    queue_name text NOT NULL,
    locked_at timestamp with time zone,
    locked_by text,
    is_available boolean GENERATED ALWAYS AS ((locked_at IS NULL)) STORED NOT NULL,
    CONSTRAINT job_queues_queue_name_check CHECK ((length(queue_name) <= 128))
);


--
-- Name: _private_known_crontabs; Type: TABLE; Schema: graphile_worker; Owner: -
--

CREATE TABLE graphile_worker._private_known_crontabs (
    identifier text NOT NULL,
    known_since timestamp with time zone NOT NULL,
    last_execution timestamp with time zone
);


--
-- Name: _private_tasks; Type: TABLE; Schema: graphile_worker; Owner: -
--

CREATE TABLE graphile_worker._private_tasks (
    id integer NOT NULL,
    identifier text NOT NULL,
    CONSTRAINT tasks_identifier_check CHECK ((length(identifier) <= 128))
);


--
-- Name: _private_workers; Type: TABLE; Schema: graphile_worker; Owner: -
--

CREATE TABLE graphile_worker._private_workers (
    id text NOT NULL,
    last_heartbeat_at timestamp with time zone NOT NULL,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    metadata jsonb
);


--
-- Name: job_queues_id_seq; Type: SEQUENCE; Schema: graphile_worker; Owner: -
--

ALTER TABLE graphile_worker._private_job_queues ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME graphile_worker.job_queues_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: jobs; Type: VIEW; Schema: graphile_worker; Owner: -
--

CREATE VIEW graphile_worker.jobs AS
 SELECT jobs.id,
    job_queues.queue_name,
    tasks.identifier AS task_identifier,
    jobs.priority,
    jobs.run_at,
    jobs.attempts,
    jobs.max_attempts,
    jobs.last_error,
    jobs.created_at,
    jobs.updated_at,
    jobs.key,
    jobs.locked_at,
    jobs.locked_by,
    jobs.revision,
    jobs.flags
   FROM ((graphile_worker._private_jobs jobs
     JOIN graphile_worker._private_tasks tasks ON ((tasks.id = jobs.task_id)))
     LEFT JOIN graphile_worker._private_job_queues job_queues ON ((job_queues.id = jobs.job_queue_id)));


--
-- Name: jobs_id_seq1; Type: SEQUENCE; Schema: graphile_worker; Owner: -
--

ALTER TABLE graphile_worker._private_jobs ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME graphile_worker.jobs_id_seq1
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: migrations; Type: TABLE; Schema: graphile_worker; Owner: -
--

CREATE TABLE graphile_worker.migrations (
    id integer NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    breaking boolean DEFAULT false NOT NULL
);


--
-- Name: tasks_id_seq; Type: SEQUENCE; Schema: graphile_worker; Owner: -
--

ALTER TABLE graphile_worker._private_tasks ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME graphile_worker.tasks_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: _sqlx_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public._sqlx_migrations (
    version bigint NOT NULL,
    description text NOT NULL,
    installed_on timestamp with time zone DEFAULT now() NOT NULL,
    success boolean NOT NULL,
    checksum bytea NOT NULL,
    execution_time bigint NOT NULL
);


--
-- Name: aggregates; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.aggregates (
    model_column_id text NOT NULL,
    capability text NOT NULL,
    difficulty_tier smallint NOT NULL,
    dimension text NOT NULL,
    ewma real NOT NULL,
    variance real DEFAULT 0 NOT NULL,
    n integer NOT NULL,
    measured boolean NOT NULL,
    matrix_version text NOT NULL,
    as_of date NOT NULL
);


--
-- Name: alerts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.alerts (
    id text NOT NULL,
    kind text NOT NULL,
    severity text NOT NULL,
    subject jsonb NOT NULL,
    message text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    acked_at timestamp with time zone,
    CONSTRAINT alerts_severity_check CHECK ((severity = ANY (ARRAY['info'::text, 'warning'::text, 'critical'::text])))
);


--
-- Name: api_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.api_keys (
    id text NOT NULL,
    tenant_id text NOT NULL,
    key_hash bytea NOT NULL,
    last4 text NOT NULL,
    name text DEFAULT ''::text NOT NULL,
    status text DEFAULT 'active'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    revoked_at timestamp with time zone,
    CONSTRAINT api_keys_status_check CHECK ((status = ANY (ARRAY['active'::text, 'revoked'::text])))
);


--
-- Name: batches; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.batches (
    id text NOT NULL,
    kind text NOT NULL,
    batch_window text NOT NULL,
    status text DEFAULT 'running'::text NOT NULL,
    lease_owner text,
    lease_expires_at timestamp with time zone,
    heartbeat_at timestamp with time zone,
    attempt integer DEFAULT 1 NOT NULL,
    completed_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT batches_status_check CHECK ((status = ANY (ARRAY['running'::text, 'completed'::text, 'skipped'::text])))
);


--
-- Name: calibration; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.calibration (
    batch_id text NOT NULL,
    capability text NOT NULL,
    brier_score real NOT NULL,
    overconfidence real NOT NULL,
    n integer NOT NULL,
    scored_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: capture_access_log; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.capture_access_log (
    id text NOT NULL,
    capture_id text NOT NULL,
    tenant_id text NOT NULL,
    actor text NOT NULL,
    action text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT capture_access_log_action_check CHECK ((action = ANY (ARRAY['replay'::text, 'read'::text, 'delete'::text])))
);


--
-- Name: capture_windows; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.capture_windows (
    id text NOT NULL,
    tenant_id text NOT NULL,
    api_key_id text,
    confidence_below real,
    starts_at timestamp with time zone NOT NULL,
    ends_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: classification_cache; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.classification_cache (
    tenant_id text NOT NULL,
    prompt_hash text NOT NULL,
    classifier_version text NOT NULL,
    classification jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: classifier_usage; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.classifier_usage (
    usage_date date NOT NULL,
    tenant_id text NOT NULL,
    calls integer DEFAULT 0 NOT NULL,
    spend_usd numeric(12,6) DEFAULT 0 NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: conversation_affinity; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.conversation_affinity (
    tenant_id text NOT NULL,
    affinity_hash text NOT NULL,
    column_id text NOT NULL,
    recommendation_id text NOT NULL,
    last_used_at timestamp with time zone NOT NULL,
    expires_at timestamp with time zone NOT NULL
);


--
-- Name: corpus_versions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.corpus_versions (
    version text NOT NULL,
    description text DEFAULT ''::text NOT NULL,
    task_count integer DEFAULT 0 NOT NULL,
    active boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: debug_captures; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.debug_captures (
    id text NOT NULL,
    tenant_id text NOT NULL,
    recommendation_id text NOT NULL,
    ciphertext bytea NOT NULL,
    nonce bytea NOT NULL,
    captured_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    window_id text,
    capture_key_version integer NOT NULL
);


--
-- Name: matrix_imports; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.matrix_imports (
    manifest_sha256 bytea NOT NULL,
    format text NOT NULL,
    matrix_version text NOT NULL,
    published_at timestamp with time zone NOT NULL,
    origin_release text NOT NULL,
    signing_kids text[] NOT NULL,
    active boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: model_columns; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.model_columns (
    id text NOT NULL,
    provider text NOT NULL,
    model_id text NOT NULL,
    endpoint text NOT NULL,
    settings jsonb DEFAULT '{}'::jsonb NOT NULL,
    status text DEFAULT 'cold_start'::text NOT NULL,
    pricing jsonb DEFAULT '{}'::jsonb NOT NULL,
    quality_source_column_id text,
    cold_start_until date,
    deprecation_date date,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    execution_aliases jsonb DEFAULT '{}'::jsonb NOT NULL,
    retention_mode text DEFAULT 'unknown'::text NOT NULL,
    retention_verified_at timestamp with time zone,
    retention_valid_until timestamp with time zone,
    retention_source text,
    execution_alias_retention jsonb DEFAULT '{}'::jsonb NOT NULL,
    param_constraints jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT model_columns_provider_check CHECK ((provider = ANY (ARRAY['anthropic'::text, 'openai'::text, 'google'::text, 'xai'::text]))),
    CONSTRAINT model_columns_retention_mode_check CHECK ((retention_mode = ANY (ARRAY['unknown'::text, 'standard'::text, 'zero'::text]))),
    CONSTRAINT model_columns_status_check CHECK ((status = ANY (ARRAY['cold_start'::text, 'active'::text, 'deprecated'::text, 'retired'::text])))
);


--
-- Name: outcomes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.outcomes (
    id text NOT NULL,
    recommendation_id text NOT NULL,
    model_used text NOT NULL,
    outcome text NOT NULL,
    escalated_to text,
    details jsonb,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    endpoint_used text DEFAULT 'direct'::text NOT NULL,
    CONSTRAINT outcomes_outcome_check CHECK ((outcome = ANY (ARRAY['success'::text, 'parse_failure'::text, 'quality_insufficient'::text, 'latency_exceeded'::text, 'provider_error'::text, 'completed_unchecked'::text, 'other'::text])))
);


--
-- Name: pools; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.pools (
    id text NOT NULL,
    tenant_id text NOT NULL,
    name text NOT NULL,
    model_column_ids text[] NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: predictions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.predictions (
    batch_id text NOT NULL,
    model_column_id text NOT NULL,
    capability text NOT NULL,
    difficulty_tier smallint NOT NULL,
    predicted_pass_prob real NOT NULL,
    source text DEFAULT 'ewma'::text NOT NULL,
    matrix_version text NOT NULL,
    locked_at timestamp with time zone NOT NULL
);


--
-- Name: recommendations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.recommendations (
    id text NOT NULL,
    tenant_id text NOT NULL,
    api_key_id text NOT NULL,
    prompt_hash text,
    request jsonb NOT NULL,
    classification jsonb NOT NULL,
    candidates jsonb NOT NULL,
    response jsonb NOT NULL,
    matrix_version text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    affinity jsonb
);


--
-- Name: runs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.runs (
    id text NOT NULL,
    batch_id text NOT NULL,
    task_id text NOT NULL,
    model_column_id text NOT NULL,
    repeat_index smallint NOT NULL,
    output text,
    tokens_in integer,
    tokens_out integer,
    latency_ms integer,
    cost numeric(12,6),
    temperature real NOT NULL,
    status text NOT NULL,
    error text,
    matrix_version text NOT NULL,
    ran_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT runs_status_check CHECK ((status = ANY (ARRAY['ok'::text, 'provider_error'::text, 'timeout'::text, 'parse_failure'::text])))
);


--
-- Name: scores; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.scores (
    run_id text NOT NULL,
    grader_idx smallint NOT NULL,
    dimension text NOT NULL,
    value real NOT NULL,
    detail jsonb,
    CONSTRAINT scores_value_check CHECK (((value >= (0)::double precision) AND (value <= (1)::double precision)))
);


--
-- Name: serve_heartbeats; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.serve_heartbeats (
    process_id text NOT NULL,
    release text NOT NULL,
    host_label text NOT NULL,
    last_seen_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: tasks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tasks (
    id text NOT NULL,
    capability text NOT NULL,
    domain_tags text[] DEFAULT '{}'::text[] NOT NULL,
    difficulty_tier smallint NOT NULL,
    difficulty_knobs jsonb DEFAULT '{}'::jsonb NOT NULL,
    prompt text NOT NULL,
    attachments jsonb DEFAULT '[]'::jsonb NOT NULL,
    temperature real DEFAULT 0 NOT NULL,
    max_tokens integer NOT NULL,
    grader_specs jsonb NOT NULL,
    dimension_weights jsonb NOT NULL,
    pass_threshold real DEFAULT 0.7 NOT NULL,
    smoke_member boolean DEFAULT false NOT NULL,
    corpus_version text NOT NULL,
    active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT tasks_difficulty_tier_check CHECK (((difficulty_tier >= 1) AND (difficulty_tier <= 5)))
);


--
-- Name: tenant_constraints; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tenant_constraints (
    tenant_id text NOT NULL,
    max_cost_per_run numeric(12,6),
    max_latency_ms integer,
    zero_retention boolean DEFAULT false NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    telemetry_opt_out boolean DEFAULT false NOT NULL,
    affinity_cost_aware boolean DEFAULT false NOT NULL
);


--
-- Name: tenant_weights; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tenant_weights (
    tenant_id text NOT NULL,
    capability text NOT NULL,
    w_quality real NOT NULL,
    w_cost real NOT NULL,
    w_latency real NOT NULL,
    quality_bar real DEFAULT 0.80 NOT NULL
);


--
-- Name: tenants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tenants (
    id text NOT NULL,
    name text NOT NULL,
    status text DEFAULT 'active'::text NOT NULL,
    prompt_hmac_key bytea NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    classification_quota_override integer,
    CONSTRAINT tenants_status_check CHECK ((status = ANY (ARRAY['active'::text, 'suspended'::text])))
);


--
-- Name: upgrades; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.upgrades (
    id text NOT NULL,
    from_release text,
    to_release text NOT NULL,
    operator text NOT NULL,
    migrations text[] DEFAULT '{}'::text[] NOT NULL,
    started_at timestamp with time zone NOT NULL,
    finished_at timestamp with time zone,
    verify_failed text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Data for Name: _private_job_queues; Type: TABLE DATA; Schema: graphile_worker; Owner: -
--

COPY graphile_worker._private_job_queues (id, queue_name, locked_at, locked_by) FROM stdin;
\.


--
-- Data for Name: _private_jobs; Type: TABLE DATA; Schema: graphile_worker; Owner: -
--

COPY graphile_worker._private_jobs (id, job_queue_id, task_id, payload, priority, run_at, attempts, max_attempts, last_error, created_at, updated_at, key, locked_at, locked_by, revision, flags) FROM stdin;
\.


--
-- Data for Name: _private_known_crontabs; Type: TABLE DATA; Schema: graphile_worker; Owner: -
--

COPY graphile_worker._private_known_crontabs (identifier, known_since, last_execution) FROM stdin;
\.


--
-- Data for Name: _private_tasks; Type: TABLE DATA; Schema: graphile_worker; Owner: -
--

COPY graphile_worker._private_tasks (id, identifier) FROM stdin;
\.


--
-- Data for Name: _private_workers; Type: TABLE DATA; Schema: graphile_worker; Owner: -
--

COPY graphile_worker._private_workers (id, last_heartbeat_at, started_at, metadata) FROM stdin;
\.


--
-- Data for Name: migrations; Type: TABLE DATA; Schema: graphile_worker; Owner: -
--

COPY graphile_worker.migrations (id, ts, breaking) FROM stdin;
1	2026-10-24 21:18:58.04257+00	t
2	2026-10-24 21:18:58.120697+00	f
3	2026-10-24 21:18:58.169019+00	t
4	2026-10-24 21:18:58.222224+00	f
5	2026-10-24 21:18:58.243524+00	f
6	2026-10-24 21:18:58.272725+00	f
7	2026-10-24 21:18:58.289197+00	f
8	2026-10-24 21:18:58.309505+00	f
9	2026-10-24 21:18:58.327316+00	f
10	2026-10-24 21:18:58.342064+00	f
11	2026-10-24 21:18:58.353609+00	t
12	2026-10-24 21:18:58.533861+00	f
13	2026-10-24 21:18:58.540129+00	t
14	2026-10-24 21:18:58.551737+00	t
15	2026-10-24 21:18:58.568286+00	f
16	2026-10-24 21:18:58.583341+00	t
17	2026-10-24 21:18:58.642878+00	f
18	2026-10-24 21:18:58.653933+00	f
19	2026-10-24 21:18:58.678787+00	f
20	2026-10-24 21:18:58.688485+00	f
\.


--
-- Data for Name: _sqlx_migrations; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public._sqlx_migrations (version, description, installed_on, success, checksum, execution_time) FROM stdin;
1	baseline	2026-10-24 21:18:57.575765+00	t	\\x77fdac8cdfed0adc46f230d0dd5e4d465c443404ca87f4c1b4fa9c887d08cc53668453024191e0c319ab444c3f481989	125598473
2	batches	2026-10-24 21:18:57.70421+00	t	\\x9750da08fde4f99200befda25c21d8add6608e5e3e03e828eb88d1f0b55fe787a906aa567e06e27deb84d0529e97e5d7	9435902
3	model columns retention	2026-10-24 21:18:57.715954+00	t	\\x886463b4efcefb9f291b623a0a6c9751e2cbd9d610ac46dc605dc8d6c2dc085b7c6dee90fb59711d3bf22a07b6507842	13153430
4	model columns param constraints	2026-10-24 21:18:57.730828+00	t	\\x5e956d87396d8bf5944f984cbdef475687f6b196fd8415e67c41f391ec4ed8293d287f9105f94eea4afb249809cc9aff	4193594
5	debug captures capture key version	2026-10-24 21:18:57.737269+00	t	\\xf06b1e0642654ef3ce2459fa77e6d81e8f872f1592aa8a1da8019f163b678fd45aeb0d49dc05955af706cb5b0e1366bf	6494752
6	outcomes completed unchecked	2026-10-24 21:18:57.74447+00	t	\\xd1c75cc096ff84adcf53036100fc2ce5018313106c8718bdf276810f0978143115a9fc4be8d158a0544fbe129a1cdb52	7345748
7	tenants classification quota override	2026-10-24 21:18:57.754278+00	t	\\xb206c1abb1ea4bd23506a695700be7884c3294c5898192dde3ccf64e14158f44a217c6ef4f5e43f6c8d796535bd65166	4033666
8	classifier usage	2026-10-24 21:18:57.758976+00	t	\\x64719778021bb625b7e655487ae1848ec87019cf55b9540e6fb88a1725c316105e4a44565b9395edf28f39600ba09a31	10689077
9	tenant constraints telemetry opt out	2026-10-24 21:18:57.772027+00	t	\\xa619d1175418d1552abd07bfddf3e0b87a5e135383f344d6a7aafefa2f9154522686710ad39fde8f711107dbaff9eb56	6577157
10	conversation affinity	2026-10-24 21:18:57.779161+00	t	\\x2ed114b8ef1e71487a5f39e99a608b6409ef7bb2ec107a8ebbe7cf92873e5bc2dfe00d5eae0916e7ce8bde2985dd980f	14675305
11	matrix imports	2026-10-24 21:18:57.795063+00	t	\\x853e27a49b8808fe4afcbcdb9e97803473f599232e04435a3cff8c4857daf66c5b58aa28058b52c3ede3b7bd1f46659c	8347815
12	upgrades	2026-10-24 21:18:57.804651+00	t	\\x77b0aa75e552c9522fe63a42753fd1fb18b51b0d6c0f0ec812ff48d6d350a1e83b0ae912581877d032c9501504a34780	14671478
\.


--
-- Data for Name: aggregates; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.aggregates (model_column_id, capability, difficulty_tier, dimension, ewma, variance, n, measured, matrix_version, as_of) FROM stdin;
col_matrix_a	structured_extraction	1	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	1	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	1	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	1	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	2	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	2	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	2	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	2	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	3	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	3	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	3	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	3	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	4	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	4	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	4	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	4	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	5	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	5	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	5	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_a	structured_extraction	5	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	1	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	1	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	1	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	1	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	2	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	2	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	2	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	2	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	3	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	3	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	3	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	3	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	4	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	4	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	4	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	4	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	5	pass_rate	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	5	quality	0.9	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	5	cost_per_run_usd	0.01	0	20	t	matrix-v1	2026-10-24
col_matrix_b	structured_extraction	5	latency_p95_ms	800	0	20	t	matrix-v1	2026-10-24
\.


--
-- Data for Name: alerts; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.alerts (id, kind, severity, subject, message, created_at, acked_at) FROM stdin;
\.


--
-- Data for Name: api_keys; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.api_keys (id, tenant_id, key_hash, last4, name, status, created_at, revoked_at) FROM stdin;
key_01M5QWFYRCM6J3JCK2ADPSGH0Y	ten_01M5QWFYR457QA2D8G8S8PCJQA	\\x85d4922c5171c8fbcb71b28295f1a9c445a4e32082ee474870b53963fffbc4ed	yuty	bootstrap	active	2026-10-24 21:18:59.320395+00	\N
key_matrix_replica_app	ten_01M5QWFYR457QA2D8G8S8PCJQA	\\x50d6a7127b06661c073909b20078ada9771c9033ce3ac3eb480ebe02ac3370b5	VfJJ	matrix-app	active	2026-10-24 21:18:59.354109+00	\N
\.


--
-- Data for Name: batches; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.batches (id, kind, batch_window, status, lease_owner, lease_expires_at, heartbeat_at, attempt, completed_at, created_at, updated_at) FROM stdin;
\.


--
-- Data for Name: calibration; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.calibration (batch_id, capability, brier_score, overconfidence, n, scored_at) FROM stdin;
\.


--
-- Data for Name: capture_access_log; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.capture_access_log (id, capture_id, tenant_id, actor, action, created_at) FROM stdin;
\.


--
-- Data for Name: capture_windows; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.capture_windows (id, tenant_id, api_key_id, confidence_below, starts_at, ends_at, created_at) FROM stdin;
\.


--
-- Data for Name: classification_cache; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.classification_cache (tenant_id, prompt_hash, classifier_version, classification, created_at) FROM stdin;
\.


--
-- Data for Name: classifier_usage; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.classifier_usage (usage_date, tenant_id, calls, spend_usd, updated_at) FROM stdin;
\.


--
-- Data for Name: conversation_affinity; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.conversation_affinity (tenant_id, affinity_hash, column_id, recommendation_id, last_used_at, expires_at) FROM stdin;
\.


--
-- Data for Name: corpus_versions; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.corpus_versions (version, description, task_count, active, created_at) FROM stdin;
\.


--
-- Data for Name: debug_captures; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.debug_captures (id, tenant_id, recommendation_id, ciphertext, nonce, captured_at, expires_at, window_id, capture_key_version) FROM stdin;
\.


--
-- Data for Name: matrix_imports; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.matrix_imports (manifest_sha256, format, matrix_version, published_at, origin_release, signing_kids, active, created_at) FROM stdin;
\.


--
-- Data for Name: model_columns; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.model_columns (id, provider, model_id, endpoint, settings, status, pricing, quality_source_column_id, cold_start_until, deprecation_date, created_at, updated_at, execution_aliases, retention_mode, retention_verified_at, retention_valid_until, retention_source, execution_alias_retention, param_constraints) FROM stdin;
col_matrix_a	anthropic	matrix-model-a	direct	{}	active	{"input_per_mtok": 1.0, "output_per_mtok": 2.0}	\N	\N	\N	2026-10-24 21:18:58.992044+00	2026-10-24 21:18:58.992044+00	{}	unknown	\N	\N	\N	{}	{}
col_matrix_b	openai	matrix-model-b	direct	{}	active	{"input_per_mtok": 1.0, "output_per_mtok": 2.0}	\N	\N	\N	2026-10-24 21:18:59.175723+00	2026-10-24 21:18:59.175723+00	{}	unknown	\N	\N	\N	{}	{}
\.


--
-- Data for Name: outcomes; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.outcomes (id, recommendation_id, model_used, outcome, escalated_to, details, created_at, endpoint_used) FROM stdin;
\.


--
-- Data for Name: pools; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.pools (id, tenant_id, name, model_column_ids, is_default, created_at, updated_at) FROM stdin;
pool_01M5QWFYR7ZJST6849VET07S4N	ten_01M5QWFYR457QA2D8G8S8PCJQA	default	{col_matrix_a,col_matrix_b}	t	2026-10-24 21:18:59.320395+00	2026-10-24 21:18:59.320395+00
\.


--
-- Data for Name: predictions; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.predictions (batch_id, model_column_id, capability, difficulty_tier, predicted_pass_prob, source, matrix_version, locked_at) FROM stdin;
\.


--
-- Data for Name: recommendations; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.recommendations (id, tenant_id, api_key_id, prompt_hash, request, classification, candidates, response, matrix_version, created_at, affinity) FROM stdin;
rec_01M5QWFZ5PQEWQN2VJG9YYMVCB	ten_01M5QWFYR457QA2D8G8S8PCJQA	key_matrix_replica_app	\N	{"mode": "quality", "workload": {"capability": "structured_extraction", "difficulty": 3}}	{"method": "declared", "capability": "structured_extraction", "confidence": 1, "difficulty": 3, "domain_tags": [], "high_stakes": false}	[{"v": 0.8, "rank": 0, "model": "matrix-model-a", "leader": true, "status": "survivor", "quality": 90, "endpoint": "direct", "measured": true, "provider": "anthropic", "column_id": "col_matrix_a", "pass_rate": 0.8999999761581421, "latency_p95_ms": 800, "prefs_satisfied": true, "cost_per_run_usd": 0.009999999776482582, "low_availability": false, "clears_difficulty": true}, {"v": 0.8, "rank": 1, "model": "matrix-model-b", "leader": true, "status": "survivor", "quality": 90, "endpoint": "direct", "measured": true, "provider": "openai", "column_id": "col_matrix_b", "pass_rate": 0.8999999761581421, "latency_p95_ms": 800, "prefs_satisfied": true, "cost_per_run_usd": 0.009999999776482582, "low_availability": false, "clears_difficulty": true}]	{"mode": "quality", "pool": "default", "measured": true, "telemetry": true, "check_spec": {"ref": "checks/extraction/v1", "type": "json_schema"}, "violations": [], "classification": {"method": "declared", "capability": "structured_extraction", "confidence": 1, "difficulty": 3, "domain_tags": [], "high_stakes": false}, "matrix_version": "", "zero_retention": false, "constraints_met": true, "recommendations": [{"model": "matrix-model-a", "score": 90, "reason": "cheapest column clearing tier 3 structured_extraction at your quality bar in this pool", "endpoint": "direct", "provider": "anthropic", "settings": {}, "column_id": "col_matrix_a", "retention_mode": "unknown", "low_availability": false, "clears_difficulty": true, "execution_aliases": {}, "param_constraints": {}, "execution_alias_retention": {}}, {"model": "matrix-model-b", "score": 90, "reason": "cross-family alternative", "endpoint": "direct", "provider": "openai", "settings": {}, "column_id": "col_matrix_b", "retention_mode": "unknown", "low_availability": false, "clears_difficulty": true, "execution_aliases": {}, "param_constraints": {}, "execution_alias_retention": {}}], "recommendation_id": "rec_01M5QWFZ5PQEWQN2VJG9YYMVCB"}		2026-10-24 21:18:59.770197+00	\N
rec_01M5QWFZQCS0VEHB9XQBV0D82B	ten_01M5QWFYR457QA2D8G8S8PCJQA	key_matrix_replica_app	\N	{"mode": "quality", "workload": {"capability": "structured_extraction", "difficulty": 3}}	{"method": "declared", "capability": "structured_extraction", "confidence": 1, "difficulty": 3, "domain_tags": [], "high_stakes": false}	[{"v": 0.8, "rank": 0, "model": "matrix-model-a", "leader": true, "status": "survivor", "quality": 90, "endpoint": "direct", "measured": true, "provider": "anthropic", "column_id": "col_matrix_a", "pass_rate": 0.8999999761581421, "latency_p95_ms": 800, "prefs_satisfied": true, "cost_per_run_usd": 0.009999999776482582, "low_availability": false, "clears_difficulty": true}, {"v": 0.8, "rank": 1, "model": "matrix-model-b", "leader": true, "status": "survivor", "quality": 90, "endpoint": "direct", "measured": true, "provider": "openai", "column_id": "col_matrix_b", "pass_rate": 0.8999999761581421, "latency_p95_ms": 800, "prefs_satisfied": true, "cost_per_run_usd": 0.009999999776482582, "low_availability": false, "clears_difficulty": true}]	{"mode": "quality", "pool": "default", "measured": true, "telemetry": true, "check_spec": {"ref": "checks/extraction/v1", "type": "json_schema"}, "violations": [], "classification": {"method": "declared", "capability": "structured_extraction", "confidence": 1, "difficulty": 3, "domain_tags": [], "high_stakes": false}, "matrix_version": "", "zero_retention": false, "constraints_met": true, "recommendations": [{"model": "matrix-model-a", "score": 90, "reason": "cheapest column clearing tier 3 structured_extraction at your quality bar in this pool", "endpoint": "direct", "provider": "anthropic", "settings": {}, "column_id": "col_matrix_a", "retention_mode": "unknown", "low_availability": false, "clears_difficulty": true, "execution_aliases": {}, "param_constraints": {}, "execution_alias_retention": {}}, {"model": "matrix-model-b", "score": 90, "reason": "cross-family alternative", "endpoint": "direct", "provider": "openai", "settings": {}, "column_id": "col_matrix_b", "retention_mode": "unknown", "low_availability": false, "clears_difficulty": true, "execution_aliases": {}, "param_constraints": {}, "execution_alias_retention": {}}], "recommendation_id": "rec_01M5QWFZQCS0VEHB9XQBV0D82B"}		2026-10-24 21:19:00.337299+00	\N
\.


--
-- Data for Name: runs; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.runs (id, batch_id, task_id, model_column_id, repeat_index, output, tokens_in, tokens_out, latency_ms, cost, temperature, status, error, matrix_version, ran_at) FROM stdin;
\.


--
-- Data for Name: scores; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.scores (run_id, grader_idx, dimension, value, detail) FROM stdin;
\.


--
-- Data for Name: serve_heartbeats; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.serve_heartbeats (process_id, release, host_label, last_seen_at, created_at) FROM stdin;
\.


--
-- Data for Name: tasks; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tasks (id, capability, domain_tags, difficulty_tier, difficulty_knobs, prompt, attachments, temperature, max_tokens, grader_specs, dimension_weights, pass_threshold, smoke_member, corpus_version, active, created_at, updated_at) FROM stdin;
\.


--
-- Data for Name: tenant_constraints; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tenant_constraints (tenant_id, max_cost_per_run, max_latency_ms, zero_retention, updated_at, telemetry_opt_out, affinity_cost_aware) FROM stdin;
\.


--
-- Data for Name: tenant_weights; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tenant_weights (tenant_id, capability, w_quality, w_cost, w_latency, quality_bar) FROM stdin;
\.


--
-- Data for Name: tenants; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tenants (id, name, status, prompt_hmac_key, created_at, updated_at, classification_quota_override) FROM stdin;
ten_01M5QWFYR457QA2D8G8S8PCJQA	matrix-replica	active	\\xe69ee3c7f9e51f92781347c8a059c18b83b94a7dc845e28209e8771f86d88835	2026-10-24 21:18:59.320395+00	2026-10-24 21:18:59.320395+00	\N
\.


--
-- Data for Name: upgrades; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.upgrades (id, from_release, to_release, operator, migrations, started_at, finished_at, verify_failed, created_at) FROM stdin;
upg_01M5QWFY50Y2NMN3SM1DY71MSH	\N	0.1.0	cli:matrix	{0001_baseline.sql,0002_batches.sql,0003_model_columns_retention.sql,0004_model_columns_param_constraints.sql,0005_debug_captures_capture_key_version.sql,0006_outcomes_completed_unchecked.sql,0007_tenants_classification_quota_override.sql,0008_classifier_usage.sql,0009_tenant_constraints_telemetry_opt_out.sql,0010_conversation_affinity.sql,0011_matrix_imports.sql,0012_upgrades.sql,"graphile_worker 000001","graphile_worker 000002","graphile_worker 000003","graphile_worker 000004","graphile_worker 000005","graphile_worker 000006","graphile_worker 000007","graphile_worker 000008","graphile_worker 000009","graphile_worker 000010","graphile_worker 000011","graphile_worker 000012","graphile_worker 000013","graphile_worker 000014","graphile_worker 000015","graphile_worker 000016","graphile_worker 000017","graphile_worker 000018","graphile_worker 000019","graphile_worker 000020"}	2026-10-24 21:18:57.471991+00	2026-10-24 21:18:58.849857+00	\N	2026-10-24 21:18:58.723139+00
\.


--
-- Name: job_queues_id_seq; Type: SEQUENCE SET; Schema: graphile_worker; Owner: -
--

SELECT pg_catalog.setval('graphile_worker.job_queues_id_seq', 1, false);


--
-- Name: jobs_id_seq1; Type: SEQUENCE SET; Schema: graphile_worker; Owner: -
--

SELECT pg_catalog.setval('graphile_worker.jobs_id_seq1', 1, false);


--
-- Name: tasks_id_seq; Type: SEQUENCE SET; Schema: graphile_worker; Owner: -
--

SELECT pg_catalog.setval('graphile_worker.tasks_id_seq', 1, false);


--
-- Name: _private_workers _private_workers_pkey; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker._private_workers
    ADD CONSTRAINT _private_workers_pkey PRIMARY KEY (id);


--
-- Name: _private_job_queues job_queues_pkey1; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker._private_job_queues
    ADD CONSTRAINT job_queues_pkey1 PRIMARY KEY (id);


--
-- Name: _private_job_queues job_queues_queue_name_key; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker._private_job_queues
    ADD CONSTRAINT job_queues_queue_name_key UNIQUE (queue_name);


--
-- Name: _private_jobs jobs_key_key1; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker._private_jobs
    ADD CONSTRAINT jobs_key_key1 UNIQUE (key);


--
-- Name: _private_jobs jobs_pkey1; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker._private_jobs
    ADD CONSTRAINT jobs_pkey1 PRIMARY KEY (id);


--
-- Name: _private_known_crontabs known_crontabs_pkey; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker._private_known_crontabs
    ADD CONSTRAINT known_crontabs_pkey PRIMARY KEY (identifier);


--
-- Name: migrations migrations_pkey; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker.migrations
    ADD CONSTRAINT migrations_pkey PRIMARY KEY (id);


--
-- Name: _private_tasks tasks_identifier_key; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker._private_tasks
    ADD CONSTRAINT tasks_identifier_key UNIQUE (identifier);


--
-- Name: _private_tasks tasks_pkey; Type: CONSTRAINT; Schema: graphile_worker; Owner: -
--

ALTER TABLE ONLY graphile_worker._private_tasks
    ADD CONSTRAINT tasks_pkey PRIMARY KEY (id);


--
-- Name: _sqlx_migrations _sqlx_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public._sqlx_migrations
    ADD CONSTRAINT _sqlx_migrations_pkey PRIMARY KEY (version);


--
-- Name: aggregates aggregates_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.aggregates
    ADD CONSTRAINT aggregates_pkey PRIMARY KEY (model_column_id, capability, difficulty_tier, dimension, as_of);


--
-- Name: alerts alerts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.alerts
    ADD CONSTRAINT alerts_pkey PRIMARY KEY (id);


--
-- Name: api_keys api_keys_key_hash_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT api_keys_key_hash_key UNIQUE (key_hash);


--
-- Name: api_keys api_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT api_keys_pkey PRIMARY KEY (id);


--
-- Name: batches batches_kind_batch_window_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.batches
    ADD CONSTRAINT batches_kind_batch_window_key UNIQUE (kind, batch_window);


--
-- Name: batches batches_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.batches
    ADD CONSTRAINT batches_pkey PRIMARY KEY (id);


--
-- Name: calibration calibration_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.calibration
    ADD CONSTRAINT calibration_pkey PRIMARY KEY (batch_id, capability);


--
-- Name: capture_access_log capture_access_log_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.capture_access_log
    ADD CONSTRAINT capture_access_log_pkey PRIMARY KEY (id);


--
-- Name: capture_windows capture_windows_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.capture_windows
    ADD CONSTRAINT capture_windows_pkey PRIMARY KEY (id);


--
-- Name: classification_cache classification_cache_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.classification_cache
    ADD CONSTRAINT classification_cache_pkey PRIMARY KEY (tenant_id, prompt_hash, classifier_version);


--
-- Name: classifier_usage classifier_usage_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.classifier_usage
    ADD CONSTRAINT classifier_usage_pkey PRIMARY KEY (usage_date, tenant_id);


--
-- Name: conversation_affinity conversation_affinity_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversation_affinity
    ADD CONSTRAINT conversation_affinity_pkey PRIMARY KEY (tenant_id, affinity_hash);


--
-- Name: corpus_versions corpus_versions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.corpus_versions
    ADD CONSTRAINT corpus_versions_pkey PRIMARY KEY (version);


--
-- Name: debug_captures debug_captures_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.debug_captures
    ADD CONSTRAINT debug_captures_pkey PRIMARY KEY (id);


--
-- Name: matrix_imports matrix_imports_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.matrix_imports
    ADD CONSTRAINT matrix_imports_pkey PRIMARY KEY (manifest_sha256);


--
-- Name: model_columns model_columns_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.model_columns
    ADD CONSTRAINT model_columns_pkey PRIMARY KEY (id);


--
-- Name: outcomes outcomes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.outcomes
    ADD CONSTRAINT outcomes_pkey PRIMARY KEY (id);


--
-- Name: pools pools_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.pools
    ADD CONSTRAINT pools_pkey PRIMARY KEY (id);


--
-- Name: pools pools_tenant_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.pools
    ADD CONSTRAINT pools_tenant_id_name_key UNIQUE (tenant_id, name);


--
-- Name: predictions predictions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.predictions
    ADD CONSTRAINT predictions_pkey PRIMARY KEY (batch_id, model_column_id, capability, difficulty_tier);


--
-- Name: recommendations recommendations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.recommendations
    ADD CONSTRAINT recommendations_pkey PRIMARY KEY (id);


--
-- Name: runs runs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT runs_pkey PRIMARY KEY (id);


--
-- Name: scores scores_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.scores
    ADD CONSTRAINT scores_pkey PRIMARY KEY (run_id, grader_idx, dimension);


--
-- Name: serve_heartbeats serve_heartbeats_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.serve_heartbeats
    ADD CONSTRAINT serve_heartbeats_pkey PRIMARY KEY (process_id);


--
-- Name: tasks tasks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_pkey PRIMARY KEY (id);


--
-- Name: tenant_constraints tenant_constraints_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_constraints
    ADD CONSTRAINT tenant_constraints_pkey PRIMARY KEY (tenant_id);


--
-- Name: tenant_weights tenant_weights_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_weights
    ADD CONSTRAINT tenant_weights_pkey PRIMARY KEY (tenant_id, capability);


--
-- Name: tenants tenants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenants
    ADD CONSTRAINT tenants_pkey PRIMARY KEY (id);


--
-- Name: upgrades upgrades_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.upgrades
    ADD CONSTRAINT upgrades_pkey PRIMARY KEY (id);


--
-- Name: _private_workers_last_heartbeat_at_idx; Type: INDEX; Schema: graphile_worker; Owner: -
--

CREATE INDEX _private_workers_last_heartbeat_at_idx ON graphile_worker._private_workers USING btree (last_heartbeat_at);


--
-- Name: jobs_main_index; Type: INDEX; Schema: graphile_worker; Owner: -
--

CREATE INDEX jobs_main_index ON graphile_worker._private_jobs USING btree (priority, run_at) INCLUDE (id, task_id, job_queue_id) WHERE (is_available = true);


--
-- Name: jobs_no_queue_index; Type: INDEX; Schema: graphile_worker; Owner: -
--

CREATE INDEX jobs_no_queue_index ON graphile_worker._private_jobs USING btree (priority, run_at) INCLUDE (id, task_id) WHERE ((is_available = true) AND (job_queue_id IS NULL));


--
-- Name: api_keys_tenant_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX api_keys_tenant_id_idx ON public.api_keys USING btree (tenant_id);


--
-- Name: conversation_affinity_expires_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX conversation_affinity_expires_at ON public.conversation_affinity USING btree (expires_at);


--
-- Name: debug_captures_expires_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX debug_captures_expires_at_idx ON public.debug_captures USING btree (expires_at);


--
-- Name: debug_captures_tenant_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX debug_captures_tenant_id_idx ON public.debug_captures USING btree (tenant_id);


--
-- Name: debug_captures_window_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX debug_captures_window_id_idx ON public.debug_captures USING btree (window_id);


--
-- Name: matrix_imports_one_active; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX matrix_imports_one_active ON public.matrix_imports USING btree (active) WHERE active;


--
-- Name: model_columns_identity; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX model_columns_identity ON public.model_columns USING btree (provider, model_id, endpoint, md5((settings)::text));


--
-- Name: outcomes_recommendation_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX outcomes_recommendation_id_idx ON public.outcomes USING btree (recommendation_id);


--
-- Name: recommendations_tenant_id_created_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX recommendations_tenant_id_created_at_idx ON public.recommendations USING btree (tenant_id, created_at DESC);


--
-- Name: recommendations_tenant_id_prompt_hash_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX recommendations_tenant_id_prompt_hash_idx ON public.recommendations USING btree (tenant_id, prompt_hash);


--
-- Name: runs_batch_id_task_id_model_column_id_repeat_index_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX runs_batch_id_task_id_model_column_id_repeat_index_idx ON public.runs USING btree (batch_id, task_id, model_column_id, repeat_index);


--
-- Name: runs_task_id_model_column_id_ran_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_task_id_model_column_id_ran_at_idx ON public.runs USING btree (task_id, model_column_id, ran_at);


--
-- Name: tasks_capability_difficulty_tier_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tasks_capability_difficulty_tier_idx ON public.tasks USING btree (capability, difficulty_tier) WHERE active;


--
-- Name: aggregates aggregates_model_column_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.aggregates
    ADD CONSTRAINT aggregates_model_column_id_fkey FOREIGN KEY (model_column_id) REFERENCES public.model_columns(id);


--
-- Name: api_keys api_keys_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT api_keys_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: capture_windows capture_windows_api_key_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.capture_windows
    ADD CONSTRAINT capture_windows_api_key_id_fkey FOREIGN KEY (api_key_id) REFERENCES public.api_keys(id);


--
-- Name: capture_windows capture_windows_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.capture_windows
    ADD CONSTRAINT capture_windows_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: classification_cache classification_cache_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.classification_cache
    ADD CONSTRAINT classification_cache_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: classifier_usage classifier_usage_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.classifier_usage
    ADD CONSTRAINT classifier_usage_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: conversation_affinity conversation_affinity_column_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversation_affinity
    ADD CONSTRAINT conversation_affinity_column_id_fkey FOREIGN KEY (column_id) REFERENCES public.model_columns(id);


--
-- Name: conversation_affinity conversation_affinity_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.conversation_affinity
    ADD CONSTRAINT conversation_affinity_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: debug_captures debug_captures_recommendation_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.debug_captures
    ADD CONSTRAINT debug_captures_recommendation_id_fkey FOREIGN KEY (recommendation_id) REFERENCES public.recommendations(id);


--
-- Name: debug_captures debug_captures_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.debug_captures
    ADD CONSTRAINT debug_captures_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: debug_captures debug_captures_window_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.debug_captures
    ADD CONSTRAINT debug_captures_window_id_fkey FOREIGN KEY (window_id) REFERENCES public.capture_windows(id) ON DELETE SET NULL;


--
-- Name: model_columns model_columns_quality_source_column_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.model_columns
    ADD CONSTRAINT model_columns_quality_source_column_id_fkey FOREIGN KEY (quality_source_column_id) REFERENCES public.model_columns(id);


--
-- Name: outcomes outcomes_recommendation_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.outcomes
    ADD CONSTRAINT outcomes_recommendation_id_fkey FOREIGN KEY (recommendation_id) REFERENCES public.recommendations(id);


--
-- Name: pools pools_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.pools
    ADD CONSTRAINT pools_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: predictions predictions_model_column_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.predictions
    ADD CONSTRAINT predictions_model_column_id_fkey FOREIGN KEY (model_column_id) REFERENCES public.model_columns(id);


--
-- Name: recommendations recommendations_api_key_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.recommendations
    ADD CONSTRAINT recommendations_api_key_id_fkey FOREIGN KEY (api_key_id) REFERENCES public.api_keys(id);


--
-- Name: recommendations recommendations_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.recommendations
    ADD CONSTRAINT recommendations_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: runs runs_model_column_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT runs_model_column_id_fkey FOREIGN KEY (model_column_id) REFERENCES public.model_columns(id);


--
-- Name: runs runs_task_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT runs_task_id_fkey FOREIGN KEY (task_id) REFERENCES public.tasks(id);


--
-- Name: scores scores_run_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.scores
    ADD CONSTRAINT scores_run_id_fkey FOREIGN KEY (run_id) REFERENCES public.runs(id);


--
-- Name: tasks tasks_corpus_version_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_corpus_version_fkey FOREIGN KEY (corpus_version) REFERENCES public.corpus_versions(version);


--
-- Name: tenant_constraints tenant_constraints_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_constraints
    ADD CONSTRAINT tenant_constraints_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: tenant_weights tenant_weights_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenant_weights
    ADD CONSTRAINT tenant_weights_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);


--
-- Name: _private_job_queues; Type: ROW SECURITY; Schema: graphile_worker; Owner: -
--

ALTER TABLE graphile_worker._private_job_queues ENABLE ROW LEVEL SECURITY;

--
-- Name: _private_jobs; Type: ROW SECURITY; Schema: graphile_worker; Owner: -
--

ALTER TABLE graphile_worker._private_jobs ENABLE ROW LEVEL SECURITY;

--
-- Name: _private_known_crontabs; Type: ROW SECURITY; Schema: graphile_worker; Owner: -
--

ALTER TABLE graphile_worker._private_known_crontabs ENABLE ROW LEVEL SECURITY;

--
-- Name: _private_tasks; Type: ROW SECURITY; Schema: graphile_worker; Owner: -
--

ALTER TABLE graphile_worker._private_tasks ENABLE ROW LEVEL SECURITY;

--
-- PostgreSQL database dump complete
--

\unrestrict PB6S55cykNQAiWacfNKwe1QN1cX9mxCJcOQYuBtHbkExGS20vPgoJ7gUgUcisfG

