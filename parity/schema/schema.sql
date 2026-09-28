--
-- PostgreSQL database dump
--

-- Dumped from database version 16.4 (Homebrew)
-- Dumped by pg_dump version 16.4 (Homebrew)

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

SET default_tablespace = '';

SET default_table_access_method = heap;

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
    window_id text
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
    CONSTRAINT model_columns_provider_check CHECK ((provider = ANY (ARRAY['anthropic'::text, 'openai'::text, 'google'::text, 'xai'::text]))),
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
    CONSTRAINT outcomes_outcome_check CHECK ((outcome = ANY (ARRAY['success'::text, 'parse_failure'::text, 'quality_insufficient'::text, 'latency_exceeded'::text, 'provider_error'::text, 'other'::text])))
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
    created_at timestamp with time zone DEFAULT now() NOT NULL
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
    updated_at timestamp with time zone DEFAULT now() NOT NULL
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
    CONSTRAINT tenants_status_check CHECK ((status = ANY (ARRAY['active'::text, 'suspended'::text])))
);


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
-- Name: api_keys_tenant_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX api_keys_tenant_id_idx ON public.api_keys USING btree (tenant_id);


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
-- PostgreSQL database dump complete
--

