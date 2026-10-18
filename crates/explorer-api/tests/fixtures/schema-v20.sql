--
-- PostgreSQL database dump
--

\restrict XepcVwpbFASNKX6oC5FNyg0zk9XmcRUdbaHHAvbqh2AkE4lj68QJCT6ly6W8f3x

-- Dumped from database version 18.6 (Ubuntu 18.6-1.pgdg24.04+2)
-- Dumped by pg_dump version 18.6 (Ubuntu 18.6-1.pgdg24.04+2)

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
-- Name: transactions_inputs; Type: TYPE; Schema: public; Owner: postgres
--

CREATE TYPE public.transactions_inputs AS (
	index smallint,
	previous_outpoint_hash bytea,
	previous_outpoint_index smallint,
	signature_script bytea,
	sig_op_count smallint,
	previous_outpoint_script bytea,
	previous_outpoint_amount bigint
);



--
-- Name: transactions_outputs; Type: TYPE; Schema: public; Owner: postgres
--

CREATE TYPE public.transactions_outputs AS (
	index smallint,
	amount bigint,
	script_public_key bytea,
	script_public_key_address text
);



SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: addresses_known; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.addresses_known (
    address character varying NOT NULL,
    name character varying NOT NULL
);



--
-- Name: addresses_transactions; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.addresses_transactions (
    address text NOT NULL,
    transaction_id bytea NOT NULL,
    block_time bigint
);



--
-- Name: balances; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.balances (
    id bigint NOT NULL,
    address character varying,
    amount bigint
);



--
-- Name: balances_id_seq; Type: SEQUENCE; Schema: public; Owner: postgres
--

CREATE SEQUENCE public.balances_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;



--
-- Name: balances_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: postgres
--

ALTER SEQUENCE public.balances_id_seq OWNED BY public.balances.id;


--
-- Name: block_parent; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.block_parent (
    block_hash bytea NOT NULL,
    parent_hash bytea NOT NULL
);



--
-- Name: blocks; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.blocks (
    hash bytea NOT NULL,
    accepted_id_merkle_root bytea,
    merge_set_blues_hashes bytea[],
    merge_set_reds_hashes bytea[],
    selected_parent_hash bytea,
    bits bigint,
    blue_score bigint,
    blue_work bytea,
    daa_score bigint,
    hash_merkle_root bytea,
    nonce bytea,
    pruning_point bytea,
    "timestamp" bigint,
    utxo_commitment bytea,
    version smallint
);



--
-- Name: blocks_transactions; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.blocks_transactions (
    block_hash bytea NOT NULL,
    transaction_id bytea NOT NULL
);



--
-- Name: distribution_snapshots; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.distribution_snapshots (
    snapshot_date date NOT NULL,
    tier character varying(32) NOT NULL,
    address_count integer NOT NULL,
    total_addresses integer NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    tier_sompi bigint
);



--
-- Name: distribution_tiers; Type: TABLE; Schema: public; Owner: karlsen_user
--

CREATE TABLE public.distribution_tiers (
    "timestamp" bigint NOT NULL,
    tier smallint NOT NULL,
    count bigint,
    amount bigint
);



--
-- Name: scripts_transactions; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.scripts_transactions (
    script_public_key bytea NOT NULL,
    transaction_id bytea NOT NULL,
    block_time bigint
);



--
-- Name: subnetworks; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.subnetworks (
    id integer NOT NULL,
    subnetwork_id character varying NOT NULL
);



--
-- Name: subnetworks_id_seq; Type: SEQUENCE; Schema: public; Owner: postgres
--

CREATE SEQUENCE public.subnetworks_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;



--
-- Name: subnetworks_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: postgres
--

ALTER SEQUENCE public.subnetworks_id_seq OWNED BY public.subnetworks.id;


--
-- Name: top_balance_snapshots; Type: TABLE; Schema: public; Owner: karlsen_user
--

CREATE TABLE public.top_balance_snapshots (
    snapshot_date date NOT NULL,
    address text NOT NULL,
    amount bigint NOT NULL
);



--
-- Name: top_scripts; Type: TABLE; Schema: public; Owner: karlsen_user
--

CREATE TABLE public.top_scripts (
    "timestamp" bigint NOT NULL,
    rank smallint NOT NULL,
    script_public_key bytea,
    amount bigint
);



--
-- Name: transactions; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.transactions (
    transaction_id bytea NOT NULL,
    subnetwork_id integer,
    hash bytea,
    mass integer,
    payload bytea,
    block_time bigint,
    inputs public.transactions_inputs[],
    outputs public.transactions_outputs[],
    outputs_spent smallint
);



--
-- Name: transactions_acceptances; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.transactions_acceptances (
    transaction_id bytea,
    block_hash bytea
);



--
-- Name: utxos_frozen_legacy; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.utxos_frozen_legacy (
    transaction_id bytea,
    index smallint,
    amount bigint,
    script_public_key bytea,
    script_public_key_address text
);



--
-- Name: vars; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.vars (
    key character varying NOT NULL,
    value text
);



--
-- Name: balances id; Type: DEFAULT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.balances ALTER COLUMN id SET DEFAULT nextval('public.balances_id_seq'::regclass);


--
-- Name: subnetworks id; Type: DEFAULT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.subnetworks ALTER COLUMN id SET DEFAULT nextval('public.subnetworks_id_seq'::regclass);


--
-- Name: addresses_known addresses_known_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.addresses_known
    ADD CONSTRAINT addresses_known_pkey PRIMARY KEY (address);


--
-- Name: addresses_transactions addresses_transactions_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.addresses_transactions
    ADD CONSTRAINT addresses_transactions_pkey PRIMARY KEY (address, transaction_id);


--
-- Name: balances balances_address_key; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.balances
    ADD CONSTRAINT balances_address_key UNIQUE (address);


--
-- Name: balances balances_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.balances
    ADD CONSTRAINT balances_pkey PRIMARY KEY (id);


--
-- Name: block_parent block_parent_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.block_parent
    ADD CONSTRAINT block_parent_pkey PRIMARY KEY (block_hash, parent_hash);


--
-- Name: blocks blocks_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.blocks
    ADD CONSTRAINT blocks_pkey PRIMARY KEY (hash);


--
-- Name: blocks_transactions blocks_transactions_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.blocks_transactions
    ADD CONSTRAINT blocks_transactions_pkey PRIMARY KEY (block_hash, transaction_id);


--
-- Name: distribution_snapshots distribution_snapshots_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.distribution_snapshots
    ADD CONSTRAINT distribution_snapshots_pkey PRIMARY KEY (snapshot_date, tier);


--
-- Name: distribution_tiers distribution_tiers_pkey; Type: CONSTRAINT; Schema: public; Owner: karlsen_user
--

ALTER TABLE ONLY public.distribution_tiers
    ADD CONSTRAINT distribution_tiers_pkey PRIMARY KEY ("timestamp", tier);


--
-- Name: scripts_transactions scripts_transactions_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.scripts_transactions
    ADD CONSTRAINT scripts_transactions_pkey PRIMARY KEY (script_public_key, transaction_id);


--
-- Name: subnetworks subnetworks_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.subnetworks
    ADD CONSTRAINT subnetworks_pkey PRIMARY KEY (id);


--
-- Name: top_balance_snapshots top_balance_snapshots_pkey; Type: CONSTRAINT; Schema: public; Owner: karlsen_user
--

ALTER TABLE ONLY public.top_balance_snapshots
    ADD CONSTRAINT top_balance_snapshots_pkey PRIMARY KEY (snapshot_date, address);


--
-- Name: top_scripts top_scripts_pkey; Type: CONSTRAINT; Schema: public; Owner: karlsen_user
--

ALTER TABLE ONLY public.top_scripts
    ADD CONSTRAINT top_scripts_pkey PRIMARY KEY ("timestamp", rank);


--
-- Name: transactions_acceptances transactions_acceptances_transaction_id_key; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.transactions_acceptances
    ADD CONSTRAINT transactions_acceptances_transaction_id_key UNIQUE (transaction_id);


--
-- Name: transactions transactions_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.transactions
    ADD CONSTRAINT transactions_pkey PRIMARY KEY (transaction_id);


--
-- Name: vars vars_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.vars
    ADD CONSTRAINT vars_pkey PRIMARY KEY (key);


--
-- Name: addresses_transactions_block_time_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX addresses_transactions_block_time_idx ON public.addresses_transactions USING btree (block_time DESC);


--
-- Name: block_parent_parent_hash_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX block_parent_parent_hash_idx ON public.block_parent USING btree (parent_hash);


--
-- Name: blocks_blue_score_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX blocks_blue_score_idx ON public.blocks USING btree (blue_score);


--
-- Name: blocks_transactions_transaction_id_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX blocks_transactions_transaction_id_idx ON public.blocks_transactions USING btree (transaction_id);


--
-- Name: idx_address; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX idx_address ON public.balances USING btree (address);


--
-- Name: idx_amount; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX idx_amount ON public.balances USING btree (amount);


--
-- Name: idx_at_address_block_time; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX idx_at_address_block_time ON public.addresses_transactions USING btree (address, block_time DESC);


--
-- Name: idx_blocks_daa_score; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX idx_blocks_daa_score ON public.blocks USING btree (daa_score DESC);


--
-- Name: idx_dist_snap_date; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX idx_dist_snap_date ON public.distribution_snapshots USING btree (snapshot_date DESC);


--
-- Name: scripts_transactions_script_public_key_block_time_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX scripts_transactions_script_public_key_block_time_idx ON public.scripts_transactions USING btree (script_public_key, block_time DESC);


--
-- Name: subnetworks_subnetwork_id_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX subnetworks_subnetwork_id_idx ON public.subnetworks USING btree (subnetwork_id);


--
-- Name: transactions_acceptances_block_hash_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX transactions_acceptances_block_hash_idx ON public.transactions_acceptances USING btree (block_hash);


--
-- Name: transactions_block_time_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX transactions_block_time_idx ON public.transactions USING btree (block_time DESC);


--
-- Name: TABLE addresses_known; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE addresses_transactions; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE balances; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: SEQUENCE balances_id_seq; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE block_parent; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE blocks; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE blocks_transactions; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE distribution_snapshots; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE scripts_transactions; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE subnetworks; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: SEQUENCE subnetworks_id_seq; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE top_balance_snapshots; Type: ACL; Schema: public; Owner: karlsen_user
--



--
-- Name: TABLE transactions; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE transactions_acceptances; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE utxos_frozen_legacy; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: TABLE vars; Type: ACL; Schema: public; Owner: postgres
--



--
-- Name: DEFAULT PRIVILEGES FOR SEQUENCES; Type: DEFAULT ACL; Schema: public; Owner: postgres
--



--
-- Name: DEFAULT PRIVILEGES FOR TABLES; Type: DEFAULT ACL; Schema: public; Owner: postgres
--



--
-- PostgreSQL database dump complete
--

\unrestrict XepcVwpbFASNKX6oC5FNyg0zk9XmcRUdbaHHAvbqh2AkE4lj68QJCT6ly6W8f3x

