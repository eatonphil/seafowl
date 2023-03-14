/// Default implementation for a Repository that factors out common
/// query patterns / SQL queries between Postgres and SQLite.
///
/// Usage:
///
/// The struct has to have certain fields, since this macro relies on them:
///
/// ```ignore
/// pub struct MyRepository {
///     pub executor: sqlx::Pool<sqlx::SqlxDatabaseType>
/// }
///
/// impl MyRepository {
///     pub const MIGRATOR: sqlx::Migrator = sqlx::migrate!("my/migrations");
///     pub const QUERIES: RepositoryQueries = RepositoryQueries {
///         all_columns_in_database: "SELECT ...",
///     }
///     pub fn interpret_error(error: sqlx::Error) -> Error {
///         // Interpret the database-specific error code and turn some sqlx errors
///         // into the Error enum values like UniqueConstraintViolation/FKConstraintViolation
///         // ...
///     }
/// }
///
/// implement_repository!(SqliteRepository)
/// ```
///
/// Gigajank alert: why are we doing this? The code between PG and SQLite is extremely similar.
/// But, I couldn't find a better way to factor it out in order to reduce duplication.
/// Here's what I tried:
///
///   - Use a generic `Pool<Any>`. This causes a weird borrow checker error when using a
///     `QueryBuilder` (https://github.com/launchbadge/sqlx/issues/1978)
///   - Make the implementation generic over any DB (that implements sqlx::Database). In that
///     case, we need to add a bunch of `where` clauses to the implementation giving constraints
///     on the argument, the query and the result types (see https://stackoverflow.com/a/70573732).
///     And, when we do that, we hit the borrow checker error again from #1.
///   - Add macros with default implementations for everything in the Repository trait and use them
///     instead of putting the whole implementation in a macro. This conflicts with the #[async_trait]
///     macro and breaks it (see https://stackoverflow.com/q/68573578). Another solution in that SO
///     question is generating the implementation functions with a macro and calling them
///     from the trait, which could work but still means we have to write out all functions in the
///     PG implementation, SQLite implementation and the macros for both variants of the implementation
///     functions (since we can't build a function that's generic over any DB)
///
/// In any case, this means we have to remove compile-time query checking (even if we duplicate the code
/// completely), see https://github.com/launchbadge/sqlx/issues/121 and
/// https://github.com/launchbadge/sqlx/issues/916.

/// Queries that are different between SQLite and PG
pub struct RepositoryQueries {
    pub latest_table_versions: &'static str,
    pub cast_timestamp: &'static str,
}

#[macro_export]
macro_rules! implement_repository {
    ($repo: ident) => {
#[async_trait]
impl Repository for $repo {
    async fn setup(&self) {
        $repo::MIGRATOR
            .run(&self.executor)
            .await
            .expect("error running migrations");
    }

    async fn get_collections_in_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Vec<String>, Error> {
        let names = sqlx::query("SELECT name FROM collection WHERE database_id = $1")
            .bind(database_id)
            .fetch(&self.executor)
            .map_ok(|row| row.get("name"))
            .try_collect()
            .await.map_err($repo::interpret_error)?;
        Ok(names)
    }
    async fn get_all_columns_in_database(
        &self,
        database_id: DatabaseId,
        table_version_ids: Option<Vec<TableVersionId>>,
    ) -> Result<Vec<AllDatabaseColumnsResult>, Error> {
        let mut builder: QueryBuilder<_> = if let Some(table_version_ids) = table_version_ids {
            let mut b = QueryBuilder::new(r#"
            WITH desired_table_versions AS (
                SELECT table_id, id FROM table_version
            "#);

            if !table_version_ids.is_empty() {
                b.push(" WHERE table_version.id IN (");
                let mut separated = b.separated(", ");
                for table_version_id in table_version_ids.into_iter() {
                    separated.push_bind(table_version_id);
                }
                separated.push_unseparated(")");
            }

            b.push(r#"
                ORDER BY table_id, creation_time DESC, id DESC
            )
            "#);

            b
        } else {
            QueryBuilder::new($repo::QUERIES.latest_table_versions)
        };

        builder.push(r#"
        SELECT
            database.name AS database_name,
            collection.name AS collection_name,
            "table".name AS table_name,
            "table".id AS table_id,
            "table".uuid AS table_uuid,
            "table".legacy AS table_legacy,
            desired_table_versions.id AS table_version_id,
            table_column.name AS column_name,
            table_column.type AS column_type
        FROM database
        INNER JOIN collection ON database.id = collection.database_id
        INNER JOIN "table" ON collection.id = "table".collection_id
        INNER JOIN desired_table_versions ON "table".id = desired_table_versions.table_id
        INNER JOIN table_column ON table_column.table_version_id = desired_table_versions.id
        WHERE database.id = "#);
        builder.push_bind(database_id);

        builder.push(r#"
        ORDER BY collection_name, table_name, table_version_id, column_name
        "#);

        let query = builder.build_query_as();
        let columns = query
            .fetch(&self.executor)
            .try_collect()
            .await
            .map_err($repo::interpret_error)?;

        Ok(columns)
    }

    async fn get_all_table_partition_columns(
        &self,
        table_version_id: TableVersionId,
    ) -> Result<Vec<AllTablePartitionColumnsResult>, Error> {
        let partitions = sqlx::query_as(
            r#"SELECT
            physical_partition.id AS table_partition_id,
            physical_partition.object_storage_id,
            physical_partition.row_count AS row_count,
            physical_partition_column.name AS column_name,
            physical_partition_column.type AS column_type,
            physical_partition_column.min_value,
            physical_partition_column.max_value,
            physical_partition_column.null_count
        FROM table_partition
        INNER JOIN physical_partition ON physical_partition.id = table_partition.physical_partition_id
        -- TODO left join?
        INNER JOIN physical_partition_column ON physical_partition_column.physical_partition_id = physical_partition.id
        WHERE table_partition.table_version_id = $1
        ORDER BY table_partition_id, physical_partition_column.id
        "#,
        ).bind(table_version_id)
        .fetch_all(&self.executor)
        .await.map_err($repo::interpret_error)?;
        Ok(partitions)
    }

    async fn create_database(&self, database_name: &str) -> Result<DatabaseId, Error> {
        let id = sqlx::query(r#"INSERT INTO database (name) VALUES ($1) RETURNING (id)"#)
            .bind(database_name)
            .fetch_one(&self.executor)
            .await.map_err($repo::interpret_error)?
            .try_get("id").map_err($repo::interpret_error)?;

        Ok(id)
    }

    async fn get_collection_id_by_name(
        &self,
        database_name: &str,
        collection_name: &str,
    ) -> Result<CollectionId, Error> {
        let id = sqlx::query(
            r#"
        SELECT collection.id
        FROM collection JOIN database ON collection.database_id = database.id
        WHERE database.name = $1 AND collection.name = $2
        "#,
        )
        .bind(database_name)
        .bind(collection_name)
        .fetch_one(&self.executor)
        .await.map_err($repo::interpret_error)?
        .try_get("id").map_err($repo::interpret_error)?;

        Ok(id)
    }

    async fn get_database_id_by_name(
        &self,
        database_name: &str,
    ) -> Result<DatabaseId, Error> {
        let id = sqlx::query(r#"SELECT id FROM database WHERE database.name = $1"#)
            .bind(database_name)
            .fetch_one(&self.executor)
            .await.map_err($repo::interpret_error)?
            .try_get("id").map_err($repo::interpret_error)?;

        Ok(id)
    }

    async fn get_table_id_by_name(
        &self,
        database_name: &str,
        collection_name: &str,
        table_name: &str,
    ) -> Result<TableId, Error> {
        let id = sqlx::query(
            r#"
        SELECT "table".id
        FROM "table"
        JOIN collection ON "table".collection_id = collection.id
        JOIN database ON collection.database_id = database.id
        WHERE database.name = $1 AND collection.name = $2 AND "table".name = $3
        "#,
        )
        .bind(database_name)
        .bind(collection_name)
        .bind(table_name)
        .fetch_one(&self.executor)
        .await.map_err($repo::interpret_error)?
        .try_get("id").map_err($repo::interpret_error)?;

        Ok(id)
    }

    async fn get_all_database_ids(&self) -> Result<Vec<(String, DatabaseId)>> {
        let all_db_ids = sqlx::query(r#"SELECT name, id FROM database"#)
            .fetch_all(&self.executor)
            .await.map_err($repo::interpret_error)?
            .iter()
            .map(|row| (row.get("name"), row.get("id")))
            .collect();

        Ok(all_db_ids)
    }

    async fn create_collection(
        &self,
        database_id: DatabaseId,
        collection_name: &str,
    ) -> Result<CollectionId, Error> {
        let id = sqlx::query(
            r#"INSERT INTO "collection" (database_id, name) VALUES ($1, $2) RETURNING (id)"#,
        ).bind(database_id).bind(collection_name)
        .fetch_one(&self.executor)
        .await.map_err($repo::interpret_error)?
        .try_get("id").map_err($repo::interpret_error)?;

        Ok(id)
    }

    async fn create_table(
        &self,
        collection_id: CollectionId,
        table_name: &str,
        schema: &Schema,
        uuid: Uuid,
    ) -> Result<(TableId, TableVersionId), Error> {
        // Create new (empty) table
        let new_table_id: i64 = sqlx::query(
            r#"INSERT INTO "table" (collection_id, name, uuid) VALUES ($1, $2, $3) RETURNING (id)"#,
        )
        .bind(collection_id)
        .bind(table_name)
        .bind(uuid)
        .fetch_one(&self.executor)
        .await.map_err($repo::interpret_error)?
        .try_get("id").map_err($repo::interpret_error)?;

        // Create initial table version
        let new_version_id: i64 = sqlx::query(
            r#"INSERT INTO table_version (table_id) VALUES ($1) RETURNING (id)"#,
        )
        .bind(new_table_id)
        .fetch_one(&self.executor)
        .await.map_err($repo::interpret_error)?
        .try_get("id").map_err($repo::interpret_error)?;

        // Create columns
        // TODO this breaks if we have more than (bind limit) columns
        if !schema.arrow_schema.fields().is_empty() {
            let mut builder: QueryBuilder<_> =
                QueryBuilder::new("INSERT INTO table_column(table_version_id, name, type) ");
            builder.push_values(schema.to_column_names_types(), |mut b, col| {
                b.push_bind(new_version_id)
                    .push_bind(col.0)
                    .push_bind(col.1);
            });

            let query = builder.build();
            query.execute(&self.executor).await.map_err($repo::interpret_error)?;
        }

        Ok((new_table_id, new_version_id))
    }

    async fn delete_old_table_versions(
        &self,
        table_id: Option<TableId>,
    ) -> Result<u64, Error> {
        let query = if let Some(table_id) = table_id {
            sqlx::query(
                "DELETE FROM table_version WHERE table_id = $1 AND id NOT IN \
                (SELECT DISTINCT first_value(id) OVER (PARTITION BY table_id ORDER BY creation_time DESC, id DESC) FROM table_version)"
            ).bind(table_id)
        } else {
            sqlx::query(
                "DELETE FROM table_version WHERE id NOT IN \
                (SELECT DISTINCT first_value(id) OVER (PARTITION BY table_id ORDER BY creation_time DESC, id DESC) FROM table_version)"
            )
        };

        let delete_result = query.execute(&self.executor)
            .await
            .map_err($repo::interpret_error)?;

        Ok(delete_result.rows_affected())
    }

    async fn create_partitions(
        &self,
        partitions: Vec<SeafowlPartition>,
    ) -> Result<Vec<PhysicalPartitionId>, Error> {
        // Create partitions

        let mut builder: QueryBuilder<_> = QueryBuilder::new(
            "INSERT INTO physical_partition(row_count, object_storage_id) ",
        );
        builder.push_values(&partitions, |mut b, r| {
            b.push_bind(r.row_count)
                .push_bind(r.object_storage_id.as_ref());
        });
        builder.push("RETURNING id");

        let query = builder.build();
        let partition_ids: Vec<PhysicalPartitionId> = query
            .fetch_all(&self.executor)
            .await.map_err($repo::interpret_error)?
            .iter()
            .flat_map(|r| r.try_get("id"))
            .collect();

        // Create partition columns

        // Make an vector of (partition_id, column)
        let columns: Vec<(PhysicalPartitionId, &PartitionColumn)> = zip(&partition_ids, &partitions)
            .flat_map(|(partition_id, partition)| {
                partition.columns.iter().map(|c| (partition_id.to_owned(), c))
            })
            .collect();

        let mut builder: QueryBuilder<_> =
        QueryBuilder::new("INSERT INTO physical_partition_column(physical_partition_id, name, type, min_value, max_value, null_count) ");
        builder.push_values(columns, |mut b, (rid, c)| {
            b.push_bind(rid)
                .push_bind(c.name.as_ref())
                .push_bind(c.r#type.as_ref())
                .push_bind(c.min_value.as_ref())
                .push_bind(c.max_value.as_ref())
                .push_bind(c.null_count);
        });

        let query = builder.build();
        query.execute(&self.executor).await.map_err($repo::interpret_error)?;

        Ok(partition_ids)
    }

    async fn append_partitions_to_table(
        &self,
        partition_ids: Vec<PhysicalPartitionId>,
        table_version_id: TableVersionId,
    ) -> Result<(), Error> {
        let mut builder: QueryBuilder<_> = QueryBuilder::new(
            "INSERT INTO table_partition(table_version_id, physical_partition_id) ",
        );
        builder.push_values(partition_ids, |mut b, rid| {
            b.push_bind(table_version_id).push_bind(rid);
        });

        let query = builder.build();
        query.execute(&self.executor).await.map_err($repo::interpret_error)?;

        Ok(())
    }

    async fn get_orphan_partition_store_ids(
        &self,
    ) -> Result<Vec<String>, Error> {
        let object_storage_ids = sqlx::query(
            "SELECT DISTINCT object_storage_id FROM physical_partition
                WHERE object_storage_id NOT IN (SELECT object_storage_id FROM physical_partition
                    WHERE id IN (SELECT physical_partition_id FROM table_partition)
            )"
        )
            .fetch(&self.executor)
            .map_ok(|row| row.get("object_storage_id"))
            .try_collect()
            .await.map_err($repo::interpret_error)?;

        Ok(object_storage_ids)
    }

    async fn delete_partitions(
        &self,
        object_storage_ids: Vec<String>,
    ) -> Result<u64, Error> {
        // We have to manually construct the query since SQLite doesn't have the proper Encode trait
        let mut builder: QueryBuilder<_> = QueryBuilder::new(
            "DELETE FROM physical_partition WHERE object_storage_id IN (",
        );
        let mut separated = builder.separated(", ");
        for id in object_storage_ids.iter() {
            separated.push_bind(id);
        }
        separated.push_unseparated(")");

        let query = builder.build();
        let delete_result = query.execute(&self.executor).await.map_err($repo::interpret_error)?;

        Ok(delete_result.rows_affected())
    }

    async fn create_new_table_version(
        &self,
        uuid: Uuid,
        version: DeltaDataTypeVersion,
    ) -> Result<TableVersionId, Error> {
        // For now we only support linear history
        let last_version_id: TableVersionId = sqlx::query(r#"SELECT max(table_version.id) AS id
                FROM table_version
                JOIN "table" ON table_version.table_id = "table".id
                WHERE "table".uuid = $1"#)
            .bind(uuid)
            .fetch_one(&self.executor)
            .await.map_err($repo::interpret_error)?
            .try_get("id").map_err($repo::interpret_error)?;

        let new_version_id = sqlx::query(
            "INSERT INTO table_version (table_id, version)
            SELECT table_id, $1 FROM table_version WHERE id = $2
            RETURNING (id)",
        )
        .bind(version)
        .bind(last_version_id)
        .fetch_one(&self.executor)
        .await.map_err($repo::interpret_error)?
        .try_get("id").map_err($repo::interpret_error)?;

        sqlx::query(
            "INSERT INTO table_column (table_version_id, name, type)
            SELECT $2, name, type FROM table_column WHERE table_version_id = $1;",
        )
        .bind(last_version_id)
        .bind(new_version_id)
        .execute(&self.executor)
        .await.map_err($repo::interpret_error)?;

        Ok(new_version_id)
    }

    async fn get_all_table_versions(
        &self,
        database_name: &str,
        table_names: Option<Vec<String>>,
    ) -> Result<Vec<TableVersionsResult>, Error> {
        let query = format!(r#"SELECT
                database.name AS database_name,
                collection.name AS collection_name,
                "table".name AS table_name,
                table_version.id AS table_version_id,
                table_version.version AS version,
                "table".legacy AS table_legacy,
                {} AS creation_time
            FROM table_version
            INNER JOIN "table" ON "table".id = table_version.table_id
            INNER JOIN collection ON collection.id = "table".collection_id
            INNER JOIN database ON database.id = collection.database_id"#,
            $repo::QUERIES.cast_timestamp.replace("timestamp_column", "table_version.creation_time")
        );

        // We have to manually construct the query since SQLite doesn't have the proper Encode trait
        let mut builder: QueryBuilder<_> = QueryBuilder::new(&query);

        builder.push(" WHERE database.name = ");
        builder.push_bind(database_name);

        if let Some(table_names) = table_names {
            if !table_names.is_empty() {
                builder.push(" AND \"table\".name IN (");
                let mut separated = builder.separated(", ");
                for table_name in table_names.into_iter() {
                    separated.push_bind(table_name);
                }
                separated.push_unseparated(")");
            }
        }

        let query = builder.build_query_as();
        let table_versions = query
            .fetch(&self.executor)
            .try_collect()
            .await
            .map_err($repo::interpret_error)?;

        Ok(table_versions)
    }

    async fn get_all_table_partitions(
        &self,
        database_name: &str,
    ) -> Result<Vec<TablePartitionsResult>> {
        let table_partitions = sqlx::query_as(
            r#"
            SELECT
                database.name AS database_name,
                collection.name AS collection_name,
                "table".name AS table_name,
                "table".legacy AS table_legacy,
                table_version.id AS table_version_id,
                physical_partition.id AS table_partition_id,
                physical_partition.object_storage_id,
                physical_partition.row_count
            FROM table_version
            INNER JOIN "table" ON "table".id = table_version.table_id
            INNER JOIN collection ON collection.id = "table".collection_id
            INNER JOIN database ON database.id = collection.database_id
            LEFT JOIN table_partition ON table_partition.table_version_id = table_version.id
            LEFT JOIN physical_partition ON physical_partition.id = table_partition.physical_partition_id
            WHERE database.name = $1;
        "#)
        .bind(database_name)
        .fetch_all(&self.executor)
        .await.map_err($repo::interpret_error)?;

        Ok(table_partitions)
    }

    async fn move_table(
        &self,
        table_id: TableId,
        new_table_name: &str,
        new_collection_id: Option<CollectionId>,
    ) -> Result<(), Error> {
        // Do RETURNING(id) here and ask for the ID back with fetch_one() to force a
        // row not found error if the table doesn't exist
        let query = if let Some(new_collection_id) = new_collection_id {
            sqlx::query("UPDATE \"table\" SET name = $1, collection_id = $2 WHERE id = $3 RETURNING id").bind(new_table_name).bind(new_collection_id).bind(table_id)
        } else {
            sqlx::query("UPDATE \"table\" SET name = $1 WHERE id = $2 RETURNING id").bind(new_table_name).bind(table_id)
        };
        query.fetch_one(&self.executor).await.map_err($repo::interpret_error)?;
        Ok(())
    }

    async fn create_function(
        &self,
        database_id: DatabaseId,
        function_name: &str,
        details: &CreateFunctionDetails,
    ) -> Result<FunctionId, Error> {
        let input_types = serde_json::to_string(&details.input_types).expect("Couldn't serialize input types!");

        let new_function_id: i64 = sqlx::query(
            r#"
        INSERT INTO "function" (database_id, name, entrypoint, language, input_types, return_type, data, volatility)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING (id);
        "#)
            .bind(database_id)
            .bind(function_name)
            .bind(details.entrypoint.clone())
            .bind(details.language.to_string())
            .bind(input_types)
            .bind(details.return_type.to_string())
            .bind(details.data.clone())
            .bind(details.volatility.to_string())
            .fetch_one(&self.executor)
            .await.map_err($repo::interpret_error)?
            .try_get("id").map_err($repo::interpret_error)?;

        Ok(new_function_id)
    }

    async fn get_all_functions_in_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Vec<AllDatabaseFunctionsResult>, Error> {
        let functions = sqlx::query_as(
            r#"
        SELECT
            name,
            id,
            entrypoint,
            language,
            input_types,
            return_type,
            data,
            volatility
        FROM function
        WHERE database_id = $1;
        "#)
        .bind(database_id)
        .fetch_all(&self.executor)
        .await.map_err($repo::interpret_error)?;

        Ok(functions)
    }

    // Drop table/collection/database

    // In these methods, return the ID back so that we get an error if the
    // table/collection/schema didn't actually exist
    async fn drop_table(&self, table_id: TableId) -> Result<(), Error> {
        self.insert_dropped_tables(Some(table_id), None, None).await?;

        sqlx::query("DELETE FROM \"table\" WHERE id = $1 RETURNING id")
            .bind(table_id)
            .fetch_one(&self.executor)
            .await.map_err($repo::interpret_error)?;
        Ok(())
    }

    async fn drop_collection(&self, collection_id: CollectionId) -> Result<(), Error> {
        self.insert_dropped_tables(None, Some(collection_id), None).await?;

        sqlx::query("DELETE FROM collection WHERE id = $1 RETURNING id")
            .bind(collection_id)
            .fetch_one(&self.executor)
            .await.map_err($repo::interpret_error)?;
        Ok(())
    }

    async fn drop_database(&self, database_id: DatabaseId) -> Result<(), Error> {
        self.insert_dropped_tables(None, None, Some(database_id)).await?;

        sqlx::query("DELETE FROM database WHERE id = $1 RETURNING id")
            .bind(database_id)
            .fetch_one(&self.executor)
            .await.map_err($repo::interpret_error)?;
        Ok(())
    }

    async fn insert_dropped_tables(
        &self,
        maybe_table_id: Option<TableId>,
        maybe_collection_id: Option<CollectionId>,
        maybe_database_id: Option<DatabaseId>,
    ) -> Result<(), Error> {
        // Currently we hard delete only legacy tables, the others are soft-deleted by moving
        // them to a special table that is used for lazy cleanup of files via `VACUUM`.
        // TODO: We could do this via a trigger, but then we'd lose the ability to actually
        // perform hard deletes at the DB-level.
        // NB: We really only need the uuid for cleanup, but we also persist db/col name on the off
        // chance that we want to add table restore/undrop at some point.
        let mut builder: QueryBuilder<_> = QueryBuilder::new(
            r#"INSERT INTO dropped_table(database_name, collection_name, table_name, uuid)
            SELECT * FROM (
                SELECT database.name, collection.name, "table".name, "table".uuid
                FROM "table"
                JOIN collection ON "table".collection_id = collection.id
                JOIN database ON collection.database_id = database.id
                WHERE "table".legacy IS FALSE AND "#,
        );

        if let Some(table_id) = maybe_table_id {
            builder.push("\"table\".id = ");
            builder.push_bind(table_id);
        } else if let Some(collection_id) = maybe_collection_id {
            builder.push("collection.id = ");
            builder.push_bind(collection_id);
        } else {
            let database_id = maybe_database_id.unwrap();
            builder.push("database.id = ");
            builder.push_bind(database_id);
        }

        builder.push(") as table_to_drop");

        let query = builder.build();
        query.execute(&self.executor).await.map_err($repo::interpret_error)?;
        Ok(())
    }

    async fn get_dropped_tables(
        &self,
        database_name: &str,
    ) -> Result<Vec<DroppedTablesResult>> {
        let query = format!(r#"SELECT
                database_name,
                collection_name,
                table_name,
                uuid,
                deletion_status,
                {} AS drop_time
            FROM dropped_table WHERE database_name = $1"#,
            $repo::QUERIES.cast_timestamp.replace("timestamp_column", "drop_time")
        );

        let dropped_tables = sqlx::query_as(&query)
        .bind(database_name)
        .fetch_all(&self.executor)
        .await.map_err($repo::interpret_error)?;

        Ok(dropped_tables)
    }

    async fn delete_dropped_table(&self, uuid: Uuid) -> Result<(), Error> {
        sqlx::query("DELETE FROM dropped_table WHERE uuid = $1 RETURNING uuid")
            .bind(uuid)
            .fetch_one(&self.executor)
            .await.map_err($repo::interpret_error)?;
        Ok(())
    }
}

};
}
