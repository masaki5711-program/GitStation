-- forge bootstrap step 2 (ADR-0018 §1): the database, owned by forge_owner.
-- Run as a superuser after 01, connected to the maintenance database:
--   psql -d postgres -v dbname=forge -f 02-create-database.sql
-- CREATE DATABASE cannot run inside a transaction: this file holds exactly
-- one statement, and fails if the database exists (create it only once).
-- Database-level privileges and role settings are applied by 03 (they are
-- not copied from a template).
\set ON_ERROR_STOP on

CREATE DATABASE :"dbname" OWNER forge_owner ENCODING 'UTF8' TEMPLATE template0;
