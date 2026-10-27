-- forge bootstrap step 2 (ADR-0018 §1): the database, owned by forge_owner.
-- Run as a superuser after 01, connected to the maintenance database:
--   psql -v ON_ERROR_STOP=1 -d postgres -v dbname=forge -f 02-create-database.sql
-- CREATE DATABASE cannot run inside a transaction: this file holds exactly
-- one statement. Database-level privileges and role settings are applied by
-- 03 (they are not copied from a template).

CREATE DATABASE :"dbname" OWNER forge_owner ENCODING 'UTF8' TEMPLATE template0;
