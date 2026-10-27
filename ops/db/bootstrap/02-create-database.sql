-- forge bootstrap step 2 (ADR-0018 §1): the database, owned by the superuser
-- running this file (not forge_owner: the database owner owns schema public,
-- may create schemas and set session defaults — ADR-0018 amendment).
-- Run as a superuser after 01, connected to the maintenance database:
--   psql -d postgres -v dbname=forge -f 02-create-database.sql
-- CREATE DATABASE cannot run inside a transaction: this file holds exactly
-- one statement, and fails if the database exists (create it only once).
-- Database-level privileges and role settings are applied by 03 (they are
-- not copied from a template).
\set ON_ERROR_STOP on

CREATE DATABASE :"dbname" ENCODING 'UTF8' TEMPLATE template0;
