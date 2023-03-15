// DataFusion bindings

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::borrow::Cow;

use datafusion::datasource::TableProvider;
use itertools::Itertools;
use std::collections::HashMap;

use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::datasource::object_store::ObjectStoreUrl;

use datafusion_proto::protobuf;

use crate::datafusion::parser::{DFParser, Statement as DFStatement};
use crate::datafusion::utils::build_schema;
use crate::object_store::http::try_prepare_http_url;
use crate::object_store::wrapped::InternalObjectStore;
use crate::utils::gc_partitions;
use crate::wasm_udf::wasm::create_udf_from_wasm;
use futures::TryStreamExt;

#[cfg(test)]
use mockall::automock;
use object_store::path::Path;

use sqlparser::ast::{
    AlterTableOperation, CreateFunctionBody, FunctionDefinition, Ident, ObjectName,
    ObjectType, SchemaName, Statement, TableFactor, TableWithJoins,
};

use arrow_schema::{DataType, TimeUnit};
use chrono::{DateTime, FixedOffset, Utc};
use std::ops::Deref;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use datafusion::common::DFSchema;
use datafusion::datasource::file_format::avro::AvroFormat;
use datafusion::datasource::file_format::csv::CsvFormat;
use datafusion::datasource::file_format::file_type::{FileCompressionType, FileType};
use datafusion::datasource::file_format::json::JsonFormat;
pub use datafusion::error::{DataFusionError as Error, Result};
use datafusion::optimizer::optimizer::Optimizer;
use datafusion::optimizer::simplify_expressions::SimplifyExpressions;
use datafusion::optimizer::type_coercion::TypeCoercion;
use datafusion::optimizer::{OptimizerContext, OptimizerRule};
use datafusion::physical_expr::execution_props::ExecutionProps;
use datafusion::physical_expr::expressions::{cast, Column};
use datafusion::physical_expr::{create_physical_expr, PhysicalExpr};
use datafusion::physical_optimizer::pruning::PruningPredicate;
use datafusion::physical_plan::filter::FilterExec;
use datafusion::physical_plan::projection::ProjectionExec;
use datafusion::scalar::ScalarValue;
use datafusion::{
    arrow::{
        datatypes::{Schema, SchemaRef},
        record_batch::RecordBatch,
    },
    datasource::file_format::{parquet::ParquetFormat, FileFormat},
    error::DataFusionError,
    execution::context::TaskContext,
    physical_plan::{
        coalesce_partitions::CoalescePartitionsExec, empty::EmptyExec,
        EmptyRecordBatchStream, ExecutionPlan, SendableRecordBatchStream,
    },
    prelude::SessionContext,
    sql::TableReference,
};
use datafusion_common::OwnedTableReference;

use datafusion_expr::logical_plan::{
    CreateCatalog, CreateCatalogSchema, CreateExternalTable, CreateMemoryTable,
    DropTable, Extension, LogicalPlan, Projection,
};
use datafusion_expr::{DmlStatement, Filter, WriteOp};
use deltalake::action::{Action, Add, Remove};
use deltalake::operations::{create::CreateBuilder, write::WriteBuilder};
use deltalake::{DeltaTable, Schema as DeltaSchema};
use log::{debug, info, warn};
use parking_lot::RwLock;
use prost::Message;
use uuid::Uuid;

use crate::catalog::{PartitionCatalog, DEFAULT_SCHEMA, STAGING_SCHEMA};
use crate::datafusion::visit::VisitorMut;
use crate::delta_rs::backports::{parquet_scan_from_actions, write_execution_plan};
#[cfg(test)]
use crate::frontend::http::tests::deterministic_uuid;
use crate::provider::{project_expressions, SeafowlTable};
use crate::wasm_udf::data_types::{get_volatility, CreateFunctionDetails};
use crate::{
    catalog::{FunctionCatalog, TableCatalog},
    data_types::DatabaseId,
    nodes::{
        CreateFunction, CreateTable, DropSchema, RenameTable, SeafowlExtensionNode,
        Vacuum,
    },
    schema::Schema as SeafowlSchema,
    version::TableVersionProcessor,
};

// Scheme used for URLs referencing the object store that we use to register
// with DataFusion's object store registry.
pub const INTERNAL_OBJECT_STORE_SCHEME: &str = "seafowl";

pub fn internal_object_store_url() -> ObjectStoreUrl {
    ObjectStoreUrl::parse(format!("{INTERNAL_OBJECT_STORE_SCHEME}://")).unwrap()
}

pub fn remove_quotes_from_ident(possibly_quoted_name: &Ident) -> Ident {
    Ident::new(&possibly_quoted_name.value)
}

pub fn remove_quotes_from_idents(column_names: &[Ident]) -> Vec<Ident> {
    column_names.iter().map(remove_quotes_from_ident).collect()
}

pub fn remove_quotes_from_object_name(name: &ObjectName) -> ObjectName {
    ObjectName(remove_quotes_from_idents(&name.0))
}

pub fn remove_quotes_from_schema_name(name: &SchemaName) -> SchemaName {
    match name {
        SchemaName::Simple(schema_name) => {
            SchemaName::Simple(remove_quotes_from_object_name(schema_name))
        }
        SchemaName::UnnamedAuthorization(_) | SchemaName::NamedAuthorization(_, _) => {
            name.to_owned()
        }
    }
}

// Serialise min/max stats in the form of a given ScalarValue using Datafusion protobufs format
pub fn scalar_value_to_bytes(value: &ScalarValue) -> Option<Vec<u8>> {
    match <&ScalarValue as TryInto<protobuf::ScalarValue>>::try_into(value) {
        Ok(proto) => Some(proto.encode_to_vec()),
        Err(error) => {
            warn!("Failed to serialise min/max value {:?}: {}", value, error);
            None
        }
    }
}

pub struct DefaultSeafowlContext {
    pub inner: SessionContext,
    pub table_catalog: Arc<dyn TableCatalog>,
    pub partition_catalog: Arc<dyn PartitionCatalog>,
    pub function_catalog: Arc<dyn FunctionCatalog>,
    pub internal_object_store: Arc<InternalObjectStore>,
    pub database: String,
    pub database_id: DatabaseId,
    pub all_database_ids: Arc<RwLock<HashMap<String, DatabaseId>>>,
    pub max_partition_size: u32,
}

/// Create an ExecutionPlan that doesn't produce any results.
/// This is used for queries that are actually run before we produce the plan,
/// since they have to manipulate catalog metadata or use async to write to it.
fn make_dummy_exec() -> Arc<dyn ExecutionPlan> {
    Arc::new(EmptyExec::new(false, SchemaRef::new(Schema::empty())))
}

pub fn is_read_only(plan: &LogicalPlan) -> bool {
    !matches!(
        plan,
        LogicalPlan::CreateExternalTable(_)
            | LogicalPlan::CreateMemoryTable(_)
            | LogicalPlan::CreateView(_)
            | LogicalPlan::CreateCatalogSchema(_)
            | LogicalPlan::CreateCatalog(_)
            | LogicalPlan::DropTable(_)
            | LogicalPlan::Analyze(_)
            | LogicalPlan::Extension(_)
    )
}

pub fn is_statement_read_only(statement: &DFStatement) -> bool {
    if let DFStatement::Statement(s) = statement {
        matches!(**s, Statement::Query(_) | Statement::Explain { .. })
    } else {
        false
    }
}

// The only reason to keep this trait around (instead of migrating all the functions directly into
// DefaultSeafowlContext), is that `create_physical_plan` would then be a recursive async function,
// which works for traits, but not for structs: https://stackoverflow.com/a/74737853
//
// The workaround would be to box a future as the return of such functions, which isn't very
// appealing atm (involves heap allocations, and is not very readable).
//
// Alternatively, if we're sure that all recursive calls can be handled by the inner (DataFusion's)
// `create_physical_plan` we could also rewrite the calls explicitly like that, and thus break the
// recursion.
#[cfg_attr(test, automock)]
#[async_trait]
pub trait SeafowlContext: Send + Sync {
    /// Parse SQL into one or more statements
    async fn parse_query(&self, sql: &str) -> Result<Vec<DFStatement>>;

    /// Create a logical plan for a query (single-statement SQL)
    async fn create_logical_plan(&self, sql: &str) -> Result<LogicalPlan>;

    /// Create a logical plan for a query from a parsed statement
    async fn create_logical_plan_from_statement(
        &self,
        statement: DFStatement,
    ) -> Result<LogicalPlan>;

    /// Create a physical plan for a query.
    /// This runs `create_logical_plan` and then `create_physical_plan`.
    /// Note that for some statements like INSERT, this will also execute
    /// the query.
    async fn plan_query(&self, sql: &str) -> Result<Arc<dyn ExecutionPlan>>;

    /// Create a physical plan from a logical plan.
    /// Note that for some statements like INSERT, this will also execute
    /// the query.
    async fn create_physical_plan(
        &self,
        plan: &LogicalPlan,
    ) -> Result<Arc<dyn ExecutionPlan>>;

    /// Execute a plan, producing a vector of results.
    async fn collect(
        &self,
        physical_plan: Arc<dyn ExecutionPlan>,
    ) -> Result<Vec<RecordBatch>>;

    /// Execute a plan, outputting its results to a table.
    async fn plan_to_table(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        schema_name: String,
        table_name: String,
    ) -> Result<()>;
}

impl DefaultSeafowlContext {
    // Create a new `DefaultSeafowlContext` with a new inner context scoped to a different default DB
    pub fn scope_to_database(&self, name: String) -> Result<Arc<DefaultSeafowlContext>> {
        let database_id =
            self.all_database_ids
                .read()
                .get(name.as_str())
                .map(|db_id| *db_id as DatabaseId)
                .ok_or_else(|| {
                    DataFusionError::Plan(format!(
                        "Unknown database {name}; try creating one with CREATE DATABASE first"
                    ))
                })?;

        // Swap the default database in the new internal context's session config
        let session_config = self
            .inner()
            .copied_config()
            .with_default_catalog_and_schema(name.clone(), DEFAULT_SCHEMA);

        Ok(Arc::from(DefaultSeafowlContext {
            inner: SessionContext::with_config_rt(
                session_config,
                self.inner().runtime_env(),
            ),
            table_catalog: self.table_catalog.clone(),
            partition_catalog: self.partition_catalog.clone(),
            function_catalog: self.function_catalog.clone(),
            internal_object_store: self.internal_object_store.clone(),
            database: name,
            database_id,
            all_database_ids: self.all_database_ids.clone(),
            max_partition_size: self.max_partition_size,
        }))
    }

    pub fn inner(&self) -> &SessionContext {
        &self.inner
    }

    /// Reload the context to apply / pick up new schema changes
    pub(crate) async fn reload_schema(&self) -> Result<()> {
        // DataFusion's catalog provider interface is not async, which means that we aren't really
        // supposed to perform IO when loading the list of schemas. On the other hand, as of DF 16
        // the schema provider allows for async fetching of tables. However, this isn't that helpful,
        // since for a query with multiple tables we'd have multiple separate DB hits to load them,
        // whereas below we load everything we need up front. (Furthermore, table existence and name
        // listing are still sync meaning we'd need the pre-load for them as well.)
        // We hence load all schemas and tables into memory before every query (otherwise writes
        // applied by a different Seafowl instance won't be visible by us).

        // This does incur a latency cost to every query.

        self.inner.register_catalog(
            &self.database,
            Arc::new(self.table_catalog.load_database(self.database_id).await?),
        );

        // Register all functions in the database
        self.function_catalog
            .get_all_functions_in_database(self.database_id)
            .await?
            .iter()
            .try_for_each(|f| self.register_function(&f.name, &f.details))
    }

    // Check that the TableReference doesn't have a database/schema in it.
    // We create all external tables in the staging schema (backed by DataFusion's
    // in-memory schema provider) instead.
    fn resolve_staging_ref(
        &self,
        name: &OwnedTableReference,
    ) -> Result<OwnedTableReference> {
        // NB: Since Datafusion 16.0.0 there's this OwnedTableReference enum and for external tables
        // the parsed ObjectName (which may be multipart, fully-qualified name) is coerced into the
        // `Bare` enum variant (see `external_table_to_plan` in datafusion-sql) for some reason.
        //
        // This means that any potential catalog/schema references get condensed into the name, so
        // we have to unravel that name here again, and then resolve it properly.
        let full_name = name.to_string();
        let reference = TableReference::from(full_name.as_str());
        let resolved_reference = reference.resolve(&self.database, STAGING_SCHEMA);

        if resolved_reference.catalog != self.database
            || resolved_reference.schema != STAGING_SCHEMA
        {
            return Err(DataFusionError::Plan(format!(
                "Can only create external tables in the staging schema.
                        Omit the schema/database altogether or use {}.{}.{}",
                &self.database, STAGING_SCHEMA, resolved_reference.table
            )));
        }

        Ok(OwnedTableReference::Full {
            catalog: resolved_reference.catalog.to_string(),
            schema: resolved_reference.schema.to_string(),
            table: resolved_reference.table.to_string(),
        })
    }

    /// Get a provider for a given table, return Err if it doesn't exist
    async fn get_table_provider(
        &self,
        table_name: impl Into<String>,
    ) -> Result<Arc<dyn TableProvider>> {
        let table_name = table_name.into();
        let table_ref = TableReference::from(table_name.as_str());

        let resolved_ref = table_ref.resolve(&self.database, DEFAULT_SCHEMA);

        self.inner
            .catalog(&resolved_ref.catalog)
            .ok_or_else(|| {
                Error::Plan(format!(
                    "failed to resolve catalog: {}",
                    resolved_ref.catalog
                ))
            })?
            .schema(&resolved_ref.schema)
            .ok_or_else(|| {
                Error::Plan(format!("failed to resolve schema: {}", resolved_ref.schema))
            })?
            .table(&resolved_ref.table)
            .await
            .ok_or_else(|| {
                Error::Plan(format!(
                    "'{}.{}.{}' not found",
                    resolved_ref.catalog, resolved_ref.schema, resolved_ref.table
                ))
            })
    }

    /// Resolve a table reference into a Seafowl table
    pub async fn try_get_seafowl_table(
        &self,
        table_name: impl Into<String> + std::fmt::Debug,
    ) -> Result<SeafowlTable> {
        let table_name = table_name.into();
        let table_provider = self.get_table_provider(&table_name).await?;

        let seafowl_table = match table_provider.as_any().downcast_ref::<SeafowlTable>() {
            Some(seafowl_table) => Ok(seafowl_table),
            None => Err(Error::Plan(format!(
                "'{table_name:?}' is a read-only table"
            ))),
        }?;
        Ok(seafowl_table.clone())
    }

    /// Resolve a table reference into a Delta table
    pub async fn try_get_delta_table<'a>(
        &self,
        table_name: impl Into<TableReference<'a>>,
    ) -> Result<DeltaTable> {
        let table_object_store = self
            .inner
            .table_provider(table_name)
            .await?
            .as_any()
            .downcast_ref::<DeltaTable>()
            .ok_or_else(|| {
                DataFusionError::Execution("Table {table_name} not found".to_string())
            })?
            .object_store();

        // We can't just keep hold of the downcasted ref from above because of
        // `temporary value dropped while borrowed`
        Ok(DeltaTable::new(table_object_store, Default::default()))
    }

    // Parse the uuid from the Delta table uri if available
    async fn get_table_uuid<'a>(
        &self,
        name: impl Into<TableReference<'a>>,
    ) -> Result<Uuid> {
        match self
            .inner
            .table_provider(name)
            .await?
            .as_any()
            .downcast_ref::<DeltaTable>()
        {
            None => {
                // TODO: try to load from DB if missing?
                Err(DataFusionError::Execution(
                    "Couldn't fetch table uuid".to_string(),
                ))
            }
            Some(delta_table) => {
                let table_uri = Path::from(delta_table.table_uri());
                let uuid = table_uri.parts().last().ok_or_else(|| {
                    DataFusionError::Execution(format!(
                        "Failed parsing the uuid suffix from uri {table_uri} for table {delta_table}"
                    ))
                })?;
                Ok(Uuid::try_parse(uuid.as_ref()).map_err(|err| {
                    DataFusionError::Execution(format!(
                        "Failed parsing uuid from {uuid:?}: {err}"
                    ))
                })?)
            }
        }
    }

    async fn create_delta_table<'a>(
        &self,
        name: impl Into<TableReference<'a>>,
        schema: &Schema,
    ) -> Result<DeltaTable> {
        let table_ref: TableReference = name.into();
        let resolved_ref = table_ref.resolve(&self.database, DEFAULT_SCHEMA);
        let schema_name = resolved_ref.schema.clone();
        let table_name = resolved_ref.table.clone();

        let sf_schema = SeafowlSchema {
            arrow_schema: Arc::new(schema.clone()),
        };
        let collection_id = self
            .table_catalog
            .get_collection_id_by_name(&self.database, &schema_name)
            .await?
            .ok_or_else(|| {
                Error::Plan(format!("Schema {schema_name:?} does not exist!"))
            })?;

        let delta_schema = DeltaSchema::try_from(schema)?;

        // TODO: we could be doing this inside the DB itself (i.e. `... DEFAULT gen_random_uuid()`
        // in Postgres and `... DEFAULT (uuid())` in SQLite) however we won't be able to do it until
        // sqlx 0.7 is released (which has libsqlite3-sys > 0.25, with the SQLite version that has
        // the `uuid()` function).
        // Then we could create the table in our catalog first and try to create the delta table itself
        // with the returned uuid (and delete the catalog entry if the object store creation fails).
        // On the other hand that would complicate etag testing logic.
        #[cfg(test)]
        let table_uuid = deterministic_uuid();
        #[cfg(not(test))]
        let table_uuid = Uuid::new_v4();
        let table_object_store = self.internal_object_store.for_delta_table(table_uuid);

        let table = CreateBuilder::new()
            .with_object_store(table_object_store)
            .with_table_name(&*table_name)
            .with_columns(delta_schema.get_fields().clone())
            .with_comment(format!(
                "Created by Seafowl version {}",
                env!("CARGO_PKG_VERSION")
            ))
            .await?;

        // We still persist the table into our own catalog, one reason is us being able to load all
        // tables and their schemas in bulk to satisfy information_schema queries.
        // Another is to keep track of table uuid's, which are used to construct the table uri.
        // We may look into doing this via delta-rs somehow eventually.
        self.table_catalog
            .create_table(collection_id, &table_name, &sf_schema, table_uuid)
            .await?;

        debug!("Created new table {table}");
        Ok(table)
    }

    // Project incompatible data types if any to delta-rs compatible ones (for now ns -> us)
    async fn coerce_plan(
        &self,
        plan: Arc<dyn ExecutionPlan>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let mut incompatible_data_type = false;
        let schema = plan.schema().as_ref().clone();
        let projection = schema
            .fields()
            .iter()
            .enumerate()
            .map(|(id, f)| {
                let col = Arc::new(Column::new(f.name(), id));
                match f.data_type() {
                    DataType::Timestamp(TimeUnit::Nanosecond, tz) => {
                        incompatible_data_type = true;
                        let data_type =
                            DataType::Timestamp(TimeUnit::Microsecond, tz.clone());
                        Ok((cast(col, &schema, data_type)?, f.name().to_string()))
                    }
                    _ => Ok((col as _, f.name().to_string())),
                }
            })
            .collect::<Result<Vec<(Arc<dyn PhysicalExpr>, String)>>>()?;

        if incompatible_data_type {
            Ok(Arc::new(ProjectionExec::try_new(projection, plan)?))
        } else {
            Ok(plan)
        }
    }

    /// Generate the Delta table builder and execute the write
    pub async fn plan_to_delta_table<'a>(
        &self,
        name: impl Into<TableReference<'a>>,
        plan: &Arc<dyn ExecutionPlan>,
    ) -> Result<DeltaTable> {
        let table_uuid = self.get_table_uuid(name).await?;
        let table_object_store = self.internal_object_store.for_delta_table(table_uuid);

        // We're not exposing `target_file_size` nor `write_batch_size` atm
        let table = WriteBuilder::new()
            .with_input_execution_plan(plan.clone())
            .with_input_session_state(self.inner.state())
            .with_object_store(table_object_store)
            .await?;

        // TODO: if `DeltaTable::get_version_timestamp` was globally public we could also pass the
        // exact version timestamp, instead of creating one automatically in our own catalog (which
        // could lead to minor timestamp differences).
        self.table_catalog
            .create_new_table_version(table_uuid, table.version())
            .await?;

        debug!("Written table version {} for {table}", table.version());
        Ok(table)
    }

    fn register_function(
        &self,
        name: &str,
        details: &CreateFunctionDetails,
    ) -> Result<()> {
        let function_code = STANDARD
            .decode(&details.data)
            .map_err(|e| Error::Execution(format!("Error decoding the UDF: {e:?}")))?;

        let function = create_udf_from_wasm(
            &details.language,
            name,
            &function_code,
            &details.entrypoint,
            &details.input_types,
            &details.return_type,
            get_volatility(&details.volatility),
        )?;
        self.inner.register_udf(function);

        Ok(())
    }

    async fn execute_stream(
        &self,
        physical_plan: Arc<dyn ExecutionPlan>,
    ) -> Result<SendableRecordBatchStream> {
        match physical_plan.output_partitioning().partition_count() {
            0 => Ok(Box::pin(EmptyRecordBatchStream::new(
                physical_plan.schema(),
            ))),
            1 => self.execute_stream_partitioned(&physical_plan, 0).await,
            _ => {
                let plan: Arc<dyn ExecutionPlan> =
                    Arc::new(CoalescePartitionsExec::new(physical_plan));
                self.execute_stream_partitioned(&plan, 0).await
            }
        }
    }

    async fn execute_stream_partitioned(
        &self,
        physical_plan: &Arc<dyn ExecutionPlan>,
        partition: usize,
    ) -> Result<SendableRecordBatchStream> {
        let task_context = Arc::new(TaskContext::from(self.inner()));
        physical_plan.execute(partition, task_context)
    }

    // Copied from DataFusion's source code (private functions)
    async fn create_external_table(
        &self,
        cmd: &CreateExternalTable,
        filter_suffix: bool,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let table_provider: Arc<dyn TableProvider> =
            if ["TABLE", "DELTATABLE"].contains(&cmd.file_type.as_str()) {
                self.create_custom_table(cmd).await?
            } else {
                // This is quite unfortunate, as the DataFusion creates everything we need above, apart from
                // the override of the `file_extension`. There's no way to override the ListingOptions
                // in the created ListingTable, so we just use a slightly modified ListingTableFactory
                // code to instantiate the table.
                self.create_listing_table(cmd, filter_suffix).await?
            };

        let table = self.inner.table(&cmd.name).await;
        match (&cmd.if_not_exists, table) {
            (true, Ok(_)) => Ok(make_dummy_exec()),
            (_, Err(_)) => {
                self.inner.register_table(&cmd.name, table_provider)?;
                return Ok(make_dummy_exec());
            }
            (false, Ok(_)) => Err(DataFusionError::Execution(format!(
                "Table '{:?}' already exists",
                cmd.name
            ))),
        }
    }

    // Copied from DataFusion's source code (private functions)
    async fn create_custom_table(
        &self,
        cmd: &CreateExternalTable,
    ) -> Result<Arc<dyn TableProvider>> {
        let state = self.inner.state();
        let file_type = cmd.file_type.to_uppercase();
        let factory = &state
            .runtime_env()
            .table_factories
            .get(file_type.as_str())
            .ok_or_else(|| {
                DataFusionError::Execution(format!(
                    "Unable to find factory for {}",
                    cmd.file_type
                ))
            })?;
        let table = (*factory).create(&state, cmd).await?;
        Ok(table)
    }

    // Copied from TableProviderFactory for the ListingTable with some minimal changes
    async fn create_listing_table(
        &self,
        cmd: &CreateExternalTable,
        filter_suffix: bool,
    ) -> Result<Arc<dyn TableProvider>> {
        let file_compression_type = FileCompressionType::from(cmd.file_compression_type);
        let file_type = FileType::from_str(cmd.file_type.as_str()).map_err(|_| {
            DataFusionError::Execution(format!("Unknown FileType {}", cmd.file_type))
        })?;

        // Change from default DataFusion behaviour: allow disabling filtering by an extension
        let file_extension = if filter_suffix {
            file_type.get_ext_with_compression(file_compression_type.to_owned())?
        } else {
            "".to_string()
        };

        let file_format: Arc<dyn FileFormat> = match file_type {
            FileType::CSV => Arc::new(
                CsvFormat::default()
                    .with_has_header(cmd.has_header)
                    .with_delimiter(cmd.delimiter as u8)
                    .with_file_compression_type(file_compression_type),
            ),
            FileType::PARQUET => Arc::new(ParquetFormat::default()),
            FileType::AVRO => Arc::new(AvroFormat::default()),
            FileType::JSON => Arc::new(
                JsonFormat::default().with_file_compression_type(file_compression_type),
            ),
        };

        let (provided_schema, table_partition_cols) = if cmd.schema.fields().is_empty() {
            (
                None,
                cmd.table_partition_cols
                    .iter()
                    .map(|x| (x.clone(), DataType::Utf8))
                    .collect::<Vec<_>>(),
            )
        } else {
            let schema: SchemaRef = Arc::new(cmd.schema.as_ref().to_owned().into());
            let table_partition_cols = cmd
                .table_partition_cols
                .iter()
                .map(|col| {
                    schema
                        .field_with_name(col)
                        .map_err(DataFusionError::ArrowError)
                })
                .collect::<datafusion_common::Result<Vec<_>>>()?
                .into_iter()
                .map(|f| (f.name().to_owned(), f.data_type().to_owned()))
                .collect();
            // exclude partition columns to support creating partitioned external table
            // with a specified column definition like
            // `create external table a(c0 int, c1 int) stored as csv partitioned by (c1)...`
            let mut project_idx = Vec::new();
            for i in 0..schema.fields().len() {
                if !cmd.table_partition_cols.contains(schema.field(i).name()) {
                    project_idx.push(i);
                }
            }
            let schema = Arc::new(schema.project(&project_idx)?);
            (Some(schema), table_partition_cols)
        };

        let state = self.inner.state();
        let options = ListingOptions::new(file_format)
            .with_collect_stat(state.config().collect_statistics())
            .with_file_extension(file_extension)
            .with_target_partitions(state.config().target_partitions())
            .with_table_partition_cols(table_partition_cols)
            .with_file_sort_order(None);

        let table_path = ListingTableUrl::parse(&cmd.location)?;
        let resolved_schema = match provided_schema {
            None => options.infer_schema(&state, &table_path).await?,
            Some(s) => s,
        };
        let config = ListingTableConfig::new(table_path)
            .with_listing_options(options)
            .with_schema(resolved_schema);
        let table =
            ListingTable::try_new(config)?.with_definition(cmd.definition.clone());
        Ok(Arc::new(table))
    }
}

#[async_trait]
impl SeafowlContext for DefaultSeafowlContext {
    async fn parse_query(&self, sql: &str) -> Result<Vec<DFStatement>> {
        Ok(DFParser::parse_sql(sql)?.into_iter().collect_vec())
    }

    async fn create_logical_plan_from_statement(
        &self,
        statement: DFStatement,
    ) -> Result<LogicalPlan> {
        // Reload the schema before planning a query
        // TODO: A couple of possible optimisations here:
        // 1. Do a visit of the statement AST, and then load the metadata for only the referenced identifiers.
        // 2. No need to load metadata for the TableProvider implementation maps when instantiating SqlToRel,
        //    since it's sufficient to have metadata for TableSource implementation in the logical query
        //    planning phase. We could use a lighter structure for that, and implement `ContextProvider` for
        //    it rather than for DefaultSeafowlContext.
        self.reload_schema().await?;
        let state = self.inner.state();

        match statement.clone() {
            DFStatement::Statement(s) => match *s {
                Statement::Query(mut q) => {
                    // Determine if some of the tables reference a non-latest version using table
                    // function syntax. If so, rename the tables in the query by appending the
                    // explicit version id for the provided timestamp and add it to the schema
                    // provider's map.

                    let mut version_processor = TableVersionProcessor::new(self.database.clone(), DEFAULT_SCHEMA.to_string());
                    version_processor.visit_query(&mut q);

                    if !version_processor.table_versions.is_empty() {
                        // Create a new session context and session state, to avoid potential race
                        // conditions leading to schema provider map leaking into other queries (and
                        // thus polluting e.g. the information_schema output), or even worse reloading
                        // the map and having the versioned query fail during execution.
                        let session_ctx = SessionContext::with_state(self.inner.state());

                        version_processor.triage_version_ids(self.database.clone(), self.table_catalog.clone()).await?;
                        // We now have table_version_ids for each table with version specified; do another
                        // run over the query AST to rewrite the table.
                        version_processor.visit_query(&mut q);
                        debug!("Time travel query rewritten to: {}", q);

                        let tables_by_version = self
                            .table_catalog
                            .load_tables_by_version(self.database_id, Some(version_processor.table_version_ids())).await?;

                        for ((table, version), table_version_id) in &version_processor.table_versions {
                            let name_with_version =
                                version_processor.table_with_version(table, version);

                            let full_table_name = table.to_string();
                            let mut resolved_ref = TableReference::from(full_table_name.as_str()).resolve(&self.database, DEFAULT_SCHEMA);

                            let table_provider_for_version: Arc<dyn TableProvider> = if let Some(table_version_id) = table_version_id {
                                // Legacy tables
                                tables_by_version[table_version_id].clone()
                            } else {
                                // We only support datetime DeltaTable version specification for start
                                let table_uuid = self.get_table_uuid(resolved_ref.clone()).await?;
                                let table_object_store =
                                    self.internal_object_store.for_delta_table(table_uuid);
                                let datetime = DateTime::<Utc>::from(DateTime::<FixedOffset>::parse_from_rfc3339(version).map_err(|_| DataFusionError::Execution(format!(
                                    "Failed to parse version {version} as RFC3339 timestamp"
                                )))?);

                                // This won't work with `InMemory` object store for now: https://github.com/apache/arrow-rs/issues/3782
                                let mut delta_table = DeltaTable::new(table_object_store, Default::default());
                                delta_table.load_with_datetime(datetime).await?;
                                Arc::from(delta_table)
                            };

                            resolved_ref.table = Cow::Borrowed(name_with_version.as_str());

                            if !session_ctx.table_exist(resolved_ref.clone())? {
                                session_ctx.register_table(resolved_ref, table_provider_for_version)?;
                            }
                        }

                        let state = session_ctx.state();
                        return state.statement_to_plan(DFStatement::Statement(Box::from(Statement::Query(q)))).await;
                    }

                    state.statement_to_plan(DFStatement::Statement(Box::from(Statement::Query(q)))).await
                },
                Statement::CreateSchema { schema_name, if_not_exists } => state.statement_to_plan(
                    DFStatement::Statement(Box::from(Statement::CreateSchema {
                        schema_name: remove_quotes_from_schema_name(&schema_name),
                        if_not_exists
                    }))
                ).await,
                // Delegate generic queries to the basic DataFusion logical planner
                // (though note EXPLAIN [our custom query] will mean we have to implement EXPLAIN ourselves)
                Statement::Explain { .. }
                | Statement::ShowVariable { .. }
                | Statement::ShowTables { .. }
                | Statement::ShowColumns { .. }
                | Statement::CreateView { .. }
                | Statement::CreateDatabase { .. } => state.statement_to_plan(statement).await,
                Statement::Insert{ .. } => {
                    let plan = state.statement_to_plan(statement).await?;
                    state.optimize(&plan)
                }
                Statement::Update {
                    table: TableWithJoins {relation: TableFactor::Table { alias: None, args: None, with_hints, .. }, joins },
                    ..
                }
                // We only support the most basic form of UPDATE (no aliases or FROM or joins)
                    if with_hints.is_empty() && joins.is_empty() => {
                    let plan = state.statement_to_plan(statement).await?;

                    // Create a custom optimizer to avoid mangling effects of some optimizers (like
                    // `CommonSubexprEliminate`) which can add nested Projection plans and rewrite
                    // expressions
                    let optimizer = Optimizer::with_rules(
                        vec![
                            Arc::new(TypeCoercion::new()),
                            Arc::new(SimplifyExpressions::new())
                        ]
                    );
                    let config = OptimizerContext::default();
                    optimizer.optimize(&plan, &config, |plan: &LogicalPlan, rule: &dyn OptimizerRule| {
                        debug!(
                            "After applying rule '{}':\n{}\n",
                            rule.name(),
                            plan.display_indent()
                        )
                    }
                    )
                },
                Statement::Delete{ .. } => {
                    let plan = state.statement_to_plan(statement).await?;
                    state.optimize(&plan)
                }
                Statement::Drop { object_type: ObjectType::Table,
                    if_exists,
                    names,
                    cascade,
                    restrict,
                    purge } => {
                    let drop = Statement::Drop {
                        object_type: ObjectType::Table,
                        if_exists,
                        names: names.iter().map(remove_quotes_from_object_name).collect(),
                        cascade,
                        restrict,
                        purge };
                    state.statement_to_plan(DFStatement::Statement(Box::from(drop))).await
                },
                Statement::Drop { object_type: ObjectType::Schema,
                    if_exists: _,
                    names,
                    cascade: _,
                    purge: _, .. } => {
                        let name = names.first().unwrap().to_string();

                        Ok(LogicalPlan::Extension(Extension {
                            node: Arc::new(SeafowlExtensionNode::DropSchema(DropSchema { name, output_schema: Arc::new(DFSchema::empty()) }))
                        }))
                    },

                // CREATE TABLE (create empty table with columns)
                Statement::CreateTable {
                    query: None,
                    name,
                    columns,
                    constraints,
                    table_properties,
                    with_options,
                    if_not_exists,
                    or_replace: _,
                    ..
                } if constraints.is_empty()
                    && table_properties.is_empty()
                    && with_options.is_empty() =>
                {
                    let schema = build_schema(columns)?;
                    Ok(LogicalPlan::Extension(Extension {
                        node: Arc::new(SeafowlExtensionNode::CreateTable(CreateTable {
                            schema,
                            name: remove_quotes_from_object_name(&name).to_string(),
                            if_not_exists,
                            output_schema: Arc::new(DFSchema::empty())
                        })),
                    }))
                },

                // ALTER TABLE ... RENAME TO
                Statement::AlterTable { name, operation: AlterTableOperation::RenameTable {table_name: new_name }} => {
                    let old_table_name = remove_quotes_from_object_name(&name).to_string();
                    let new_table_name = remove_quotes_from_object_name(&new_name).to_string();

                    if self.get_table_provider(old_table_name.to_owned()).await.is_err() {
                        return Err(Error::Plan(
                            format!("Source table {old_table_name:?} doesn't exist")
                        ))
                    } else if self.get_table_provider(new_table_name.to_owned()).await.is_ok() {
                        return Err(Error::Plan(
                            format!("Target table {new_table_name:?} already exists")
                        ))
                    }

                    Ok(LogicalPlan::Extension(Extension {
                        node: Arc::new(SeafowlExtensionNode::RenameTable(RenameTable {
                            old_name: old_table_name,
                            new_name: new_table_name,
                            output_schema: Arc::new(DFSchema::empty())
                        })),
                    }))
                }

                // Other CREATE TABLE: SqlToRel only allows CreateTableAs statements and makes
                // a CreateMemoryTable node. We're fine with that, but we'll execute it differently.
                Statement::CreateTable { .. } => state.statement_to_plan(statement).await,

                Statement::CreateFunction {
                    temporary: false,
                    name,
                    params: CreateFunctionBody { as_: Some( FunctionDefinition::SingleQuotedDef(details) ), .. },
                    ..
                } => {
                    // We abuse the fact that in CREATE FUNCTION AS [class_name], class_name can be an arbitrary string
                    // and so we can get the user to put some JSON in there
                    let function_details: CreateFunctionDetails = serde_json::from_str(&details)
                        .map_err(|e| {
                            Error::Execution(format!("Error parsing UDF details: {e:?}"))
                        })?;

                        Ok(LogicalPlan::Extension(Extension {
                            node: Arc::new(SeafowlExtensionNode::CreateFunction(CreateFunction {
                                name: name.to_string(),
                                details: function_details,
                                output_schema: Arc::new(DFSchema::empty())
                            })),
                        }))
                    },
                Statement::Truncate { table_name, partitions} => {
                    let table_name = table_name.to_string();
                    let table_id = if partitions.is_none() && !table_name.is_empty() {
                        match self.try_get_seafowl_table(&table_name).await {
                            Ok(seafowl_table) => Some(seafowl_table.table_id),
                            Err(_) => return Err(Error::Internal(format!(
                                "Table with name {table_name} not found"
                            )))
                        }
                    } else {
                        None
                    };

                    Ok(LogicalPlan::Extension(Extension {
                        node: Arc::new(SeafowlExtensionNode::Vacuum(Vacuum {
                            partitions: partitions.is_some(),
                            table_id,
                            output_schema: Arc::new(DFSchema::empty())
                        })),
                    }))
                }
                _ => Err(Error::NotImplemented(format!(
                    "Unsupported SQL statement: {s:?}"
                ))),
            },
            DFStatement::DescribeTableStmt(_) | DFStatement::CreateExternalTable(_) => {
                state.statement_to_plan(statement).await
            }
        }
    }

    async fn create_logical_plan(&self, sql: &str) -> Result<LogicalPlan> {
        let mut statements = self.parse_query(sql).await?;

        if statements.len() != 1 {
            return Err(Error::NotImplemented(
                "The context currently only supports a single SQL statement".to_string(),
            ));
        }

        self.create_logical_plan_from_statement(statements.pop().unwrap())
            .await
    }

    async fn plan_query(&self, sql: &str) -> Result<Arc<dyn ExecutionPlan>> {
        let logical_plan = self.create_logical_plan(sql).await?;
        self.create_physical_plan(&logical_plan).await
    }

    async fn create_physical_plan(
        &self,
        plan: &LogicalPlan,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        // Similarly to DataFrame::sql, run certain logical plans outside of the actual execution flow
        // and produce a dummy physical plan instead
        match plan {
            // CREATE EXTERNAL TABLE copied from DataFusion's source code
            // It uses ListingTable which queries data at a given location using the ObjectStore
            // abstraction (URL: scheme://some-path.to.file.parquet) and it's easier to reuse this
            // mechanism in our case too.
            LogicalPlan::CreateExternalTable(
                cmd @ CreateExternalTable {
                    ref name,
                    ref location,
                    ..
                },
            ) => {
                // Replace the table name with the fully qualified one that has our staging schema
                let mut cmd = cmd.clone();
                cmd.name = self.resolve_staging_ref(name)?;

                let (location, is_http) = match try_prepare_http_url(location) {
                    Some(new_loc) => (new_loc, true),
                    None => (location.into(), false),
                };

                // Disallow the seafowl:// scheme (which is registered with DataFusion as our internal
                // object store but shouldn't be accessible via CREATE EXTERNAL TABLE)
                if location
                    .starts_with(format!("{INTERNAL_OBJECT_STORE_SCHEME}://").as_str())
                {
                    return Err(DataFusionError::Plan(format!(
                        "Invalid URL scheme for location {location:?}"
                    )));
                }

                // try_prepare_http_url changes the url in case of the HTTP object store
                // (to route _all_ HTTP URLs to our object store, not just specific hosts),
                // so inject it into the CreateExternalTable command as well.
                cmd.location = location;

                self.create_external_table(&cmd, !is_http).await
            }
            LogicalPlan::CreateCatalogSchema(CreateCatalogSchema {
                schema_name,
                if_not_exists: _,
                schema: _,
            }) => {
                // CREATE SCHEMA
                // Create a schema and register it
                self.table_catalog
                    .create_collection(self.database_id, schema_name)
                    .await?;
                Ok(make_dummy_exec())
            }
            LogicalPlan::CreateCatalog(CreateCatalog {
                catalog_name,
                if_not_exists,
                ..
            }) => {
                if self
                    .table_catalog
                    .get_database_id_by_name(catalog_name)
                    .await?
                    .is_some()
                {
                    if !*if_not_exists {
                        return Err(DataFusionError::Plan(format!(
                            "Database {catalog_name} already exists"
                        )));
                    } else {
                        return Ok(make_dummy_exec());
                    }
                }

                // Persist DB into metadata catalog
                let database_id =
                    self.table_catalog.create_database(catalog_name).await?;

                // Create the corresponding default schema as well
                self.table_catalog
                    .create_collection(database_id, DEFAULT_SCHEMA)
                    .await?;

                // Update the shared in-memory map of DB names -> ids
                self.all_database_ids
                    .write()
                    .insert(catalog_name.clone(), database_id);

                Ok(make_dummy_exec())
            }
            LogicalPlan::CreateMemoryTable(CreateMemoryTable {
                name,
                input,
                if_not_exists: _,
                or_replace: _,
            }) => {
                // This is actually CREATE TABLE AS
                let plan = self.create_physical_plan(input).await?;
                let plan = self.coerce_plan(plan).await?;

                // First create the table and then insert the data from the subquery
                // TODO: this means we'll have 2 table versions at the end, 1st from the create
                // and 2nd from the insert, while it seems more reasonable that in this case we have
                // only one
                let _table = self
                    .create_delta_table(name, plan.schema().as_ref())
                    .await?;
                self.reload_schema().await?;
                self.plan_to_delta_table(name, &plan).await?;

                Ok(make_dummy_exec())
            }
            LogicalPlan::Dml(DmlStatement {
                table_name,
                op: WriteOp::Insert,
                input,
                ..
            }) => {
                let physical = self.create_physical_plan(input).await?;

                self.plan_to_delta_table(table_name, &physical).await?;

                Ok(make_dummy_exec())
            }
            LogicalPlan::Dml(DmlStatement {
                table_name,
                op: WriteOp::Update,
                input,
                ..
            }) => {
                // Destructure input into projection expressions and the upstream scan/filter plan
                let LogicalPlan::Projection(Projection { expr, input, .. }) = &**input
                    else { return Err(DataFusionError::Plan("Update plan doesn't contain a Projection node".to_string())) };

                // TODO: Once https://github.com/delta-io/delta-rs/issues/1126 is closed use the
                // native delta-rs UPDATE op

                let mut table = self.try_get_delta_table(table_name).await?;
                table.load().await?;

                let schema_ref = TableProvider::schema(&table);
                let df_schema = DFSchema::try_from_qualified_schema(
                    table_name.table(),
                    schema_ref.as_ref(),
                )?;

                let selection_expr =
                    if let LogicalPlan::Filter(Filter { predicate, .. }) = &**input {
                        Some(create_physical_expr(
                            &predicate.clone(),
                            &df_schema,
                            schema_ref.as_ref(),
                            &ExecutionProps::new(),
                        )?)
                    } else {
                        None
                    };

                let projections = project_expressions(
                    expr,
                    &df_schema,
                    schema_ref.as_ref(),
                    selection_expr,
                )?;
                let state = self.inner.state();

                let (filters, removes) =
                    if let LogicalPlan::Filter(Filter { predicate, .. }) = &**input {
                        // A WHERE clause has been used; employ it to prune the update down to only
                        // a subset of files, while inheriting the rest from the previous version
                        let pruning_predicate = PruningPredicate::try_new(
                            predicate.clone(),
                            schema_ref.clone(),
                        )?;
                        let prune_map = pruning_predicate.prune(&table)?;

                        let files_to_prune = table
                            .get_state()
                            .files()
                            .iter()
                            .zip(prune_map.into_iter())
                            .filter_map(
                                |(add, keep)| if keep { Some(add.clone()) } else { None },
                            )
                            .collect::<Vec<Add>>();

                        (vec![predicate.clone().not()], files_to_prune)
                    } else {
                        // If no qualifier is specified we're basically updating the whole table.
                        (vec![], table.get_state().files().clone())
                    };

                if removes.is_empty() {
                    // Nothing to update
                    return Ok(make_dummy_exec());
                }

                let base_scan = parquet_scan_from_actions(
                    &table,
                    removes.as_slice(),
                    schema_ref.as_ref(),
                    filters.as_slice(),
                    &state,
                    None,
                    None,
                )
                .await?;

                // Apply the provided assignments
                let update_plan =
                    Arc::new(ProjectionExec::try_new(projections.clone(), base_scan)?);

                // Write the updated data
                let adds = write_execution_plan(
                    &table,
                    state,
                    update_plan,
                    vec![],
                    table.object_store(),
                    None,
                    None,
                )
                .await?;

                let deletion_timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;

                let mut actions: Vec<Action> =
                    adds.into_iter().map(Action::add).collect();
                for remove in removes {
                    actions.push(Action::remove(Remove {
                        path: remove.path,
                        deletion_timestamp: Some(deletion_timestamp),
                        data_change: true,
                        extended_file_metadata: Some(true),
                        partition_values: Some(remove.partition_values),
                        size: Some(remove.size),
                        tags: None,
                    }))
                }

                let mut tx = table.create_transaction(None);
                tx.add_actions(actions);
                let version = tx.commit(None, None).await?;
                let uuid = self.get_table_uuid(table_name).await?;
                self.table_catalog
                    .create_new_table_version(uuid, version)
                    .await?;

                Ok(make_dummy_exec())
            }
            LogicalPlan::Dml(DmlStatement {
                table_name,
                table_schema,
                op: WriteOp::Delete,
                input,
            }) => {
                // TODO: Once https://github.com/delta-io/delta-rs/pull/1176 is merged use that instead

                let mut table = self.try_get_delta_table(table_name).await?;
                table.load().await?;
                let schema_ref = SchemaRef::from(table_schema.deref().clone());

                let (adds, removes) = if let LogicalPlan::Filter(Filter {
                    predicate,
                    ..
                }) = &**input
                {
                    // A WHERE clause has been used; employ it to prune the filtration
                    // down to only a subset of partitions, re-use the rest as is

                    let state = self.inner.state();

                    // To simulate the effect of a WHERE clause from a DELETE, we
                    // need to use the inverse clause in a SELECT when filtering
                    let filter = create_physical_expr(
                        &predicate.clone().not(),
                        table_schema,
                        schema_ref.as_ref(),
                        &ExecutionProps::new(),
                    )?;

                    let pruning_predicate =
                        PruningPredicate::try_new(predicate.clone(), schema_ref.clone())?;
                    let prune_map = pruning_predicate.prune(&table)?;
                    let files_to_prune = table
                        .get_state()
                        .files()
                        .iter()
                        .zip(prune_map.into_iter())
                        .filter_map(
                            |(add, keep)| if keep { Some(add.clone()) } else { None },
                        )
                        .collect::<Vec<Add>>();

                    let base_scan = parquet_scan_from_actions(
                        &table,
                        files_to_prune.as_slice(),
                        schema_ref.as_ref(),
                        &[predicate.clone().not()],
                        &state,
                        None,
                        None,
                    )
                    .await?;

                    let filter_plan = Arc::new(FilterExec::try_new(filter, base_scan)?);

                    // Write the filtered out data
                    let adds = write_execution_plan(
                        &table,
                        state,
                        filter_plan,
                        vec![],
                        table.object_store(),
                        None,
                        None,
                    )
                    .await?;

                    (adds, files_to_prune)
                } else {
                    // If no qualifier is specified we're basically truncating the table.
                    // Remove all files.
                    (vec![], table.get_state().files().clone())
                };

                let deletion_timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;

                let mut actions: Vec<Action> =
                    adds.into_iter().map(Action::add).collect();
                for remove in removes {
                    actions.push(Action::remove(Remove {
                        path: remove.path,
                        deletion_timestamp: Some(deletion_timestamp),
                        data_change: true,
                        extended_file_metadata: Some(true),
                        partition_values: Some(remove.partition_values),
                        size: Some(remove.size),
                        tags: None,
                    }))
                }

                let mut tx = table.create_transaction(None);
                tx.add_actions(actions);
                let version = tx.commit(None, None).await?;
                let uuid = self.get_table_uuid(table_name).await?;
                self.table_catalog
                    .create_new_table_version(uuid, version)
                    .await?;

                Ok(make_dummy_exec())
            }
            LogicalPlan::DropTable(DropTable {
                name,
                if_exists: _,
                schema: _,
            }) => {
                // DROP TABLE
                if let Ok(table) = self.try_get_seafowl_table(name.to_string()).await {
                    // Drop for legacy tables
                    self.table_catalog.drop_table(table.table_id).await?;
                    return Ok(make_dummy_exec());
                };

                let table_ref = TableReference::from(name);
                let resolved_ref = table_ref.resolve(&self.database, DEFAULT_SCHEMA);

                let table_id = self
                    .table_catalog
                    .get_table_id_by_name(
                        &resolved_ref.catalog,
                        &resolved_ref.schema,
                        &resolved_ref.table,
                    )
                    .await?
                    .ok_or_else(|| {
                        DataFusionError::Execution("Table {name} not found".to_string())
                    })?;

                self.table_catalog.drop_table(table_id).await?;
                Ok(make_dummy_exec())
            }
            LogicalPlan::CreateView(_) => {
                return Err(Error::Plan(
                    "Creating views is currently unsupported!".to_string(),
                ))
            }
            LogicalPlan::Extension(Extension { ref node }) => {
                // Other custom nodes we made like CREATE TABLE/INSERT/ALTER
                match SeafowlExtensionNode::from_dynamic(node) {
                    Some(sfe_node) => match sfe_node {
                        SeafowlExtensionNode::CreateTable(CreateTable {
                            schema,
                            name,
                            ..
                        }) => {
                            self.create_delta_table(name.as_str(), schema).await?;

                            Ok(make_dummy_exec())
                        }
                        SeafowlExtensionNode::CreateFunction(CreateFunction {
                            name,
                            details,
                            output_schema: _,
                        }) => {
                            self.register_function(name, details)?;

                            // Persist the function in the metadata storage
                            self.function_catalog
                                .create_function(self.database_id, name, details)
                                .await?;

                            Ok(make_dummy_exec())
                        }
                        SeafowlExtensionNode::RenameTable(RenameTable {
                            old_name,
                            new_name,
                            ..
                        }) => {
                            // Resolve new table reference
                            let new_table_ref = TableReference::from(new_name.as_str());
                            let resolved_new_ref =
                                new_table_ref.resolve(&self.database, DEFAULT_SCHEMA);
                            if resolved_new_ref.catalog != self.database {
                                return Err(Error::Plan(
                                    "Changing the table's database is not supported!"
                                        .to_string(),
                                ));
                            }

                            // Resolve old table reference and fetch the table id
                            let old_table_ref = TableReference::from(old_name.as_str());
                            let resolved_old_ref =
                                old_table_ref.resolve(&self.database, DEFAULT_SCHEMA);

                            let table_id = self
                                .table_catalog
                                .get_table_id_by_name(
                                    &resolved_old_ref.catalog,
                                    &resolved_old_ref.schema,
                                    &resolved_old_ref.table,
                                )
                                .await?
                                .ok_or_else(|| {
                                    DataFusionError::Execution(
                                        "Table {old_name} not found".to_string(),
                                    )
                                })?;

                            // If the old and new table schema is different check that the
                            // corresponding collection already exists
                            let new_schema_id =
                                if resolved_new_ref.schema != resolved_old_ref.schema {
                                    let collection_id = self
                                        .table_catalog
                                        .get_collection_id_by_name(
                                            &self.database,
                                            &resolved_new_ref.schema,
                                        )
                                        .await?
                                        .ok_or_else(|| {
                                            Error::Plan(format!(
                                                "Schema \"{}\" does not exist!",
                                                &resolved_new_ref.schema,
                                            ))
                                        })?;
                                    Some(collection_id)
                                } else {
                                    None
                                };

                            // Finally update our catalog entry
                            self.table_catalog
                                .move_table(
                                    table_id,
                                    &resolved_new_ref.table,
                                    new_schema_id,
                                )
                                .await?;

                            // TODO: Update table metadata with the new table name during writes,
                            Ok(make_dummy_exec())
                        }
                        SeafowlExtensionNode::DropSchema(DropSchema { name, .. }) => {
                            if let Some(collection_id) = self
                                .table_catalog
                                .get_collection_id_by_name(&self.database, name)
                                .await?
                            {
                                self.table_catalog.drop_collection(collection_id).await?
                            };

                            Ok(make_dummy_exec())
                        }
                        SeafowlExtensionNode::Vacuum(Vacuum {
                            partitions,
                            table_id,
                            ..
                        }) => {
                            if *partitions {
                                gc_partitions(self).await
                            } else {
                                match self
                                    .table_catalog
                                    .delete_old_table_versions(*table_id)
                                    .await
                                {
                                    Ok(row_count) => {
                                        info!("Deleted {} old table versions, cleaning up partitions", row_count);
                                        gc_partitions(self).await
                                    }
                                    Err(error) => {
                                        return Err(Error::Internal(format!(
                                        "Failed to delete old table versions: {error:?}"
                                    )))
                                    }
                                }
                            }

                            Ok(make_dummy_exec())
                        }
                    },
                    None => self.inner.state().create_physical_plan(plan).await,
                }
            }
            _ => self.inner.state().create_physical_plan(plan).await,
        }
    }

    // Copied from DataFusion's physical_plan
    async fn collect(
        &self,
        physical_plan: Arc<dyn ExecutionPlan>,
    ) -> Result<Vec<RecordBatch>> {
        let stream = self.execute_stream(physical_plan).await?;
        stream.err_into().try_collect().await
    }

    /// Create a new table and insert data generated by the provided execution plan
    async fn plan_to_table(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        schema_name: String,
        table_name: String,
    ) -> Result<()> {
        // Reload the schema since `try_get_seafowl_table` relies on using DataFusion's
        // TableProvider interface (which we need to pre-populate with up to date
        // information on our tables)
        self.reload_schema().await?;

        let plan = self.coerce_plan(plan).await?;

        // Check whether table already exists and ensure that the schema exists
        let table_exists = match self
            .inner
            .catalog(&self.database)
            .ok_or_else(|| Error::Plan(format!("Database {} not found!", self.database)))?
            .schema(&schema_name)
        {
            Some(_) => {
                if self
                    .try_get_seafowl_table(format!("{schema_name}.{table_name}"))
                    .await
                    .is_ok()
                {
                    return Err(DataFusionError::Execution(
                        "Cannot insert into legacy table, please use a different name"
                            .to_string(),
                    ));
                }

                // Schema exists, check if existing table's schema matches the new one
                match self.get_table_provider(&table_name).await {
                    Ok(table) => {
                        if table.schema() != plan.schema() {
                            return Err(DataFusionError::Execution(
                                format!(
                                    "The table {table_name} already exists but has a different schema than the one provided.")
                            )
                            );
                        }

                        true
                    }
                    Err(_) => false,
                }
            }
            None => {
                // Schema doesn't exist; create one first
                self.table_catalog
                    .create_collection(self.database_id, &schema_name)
                    .await?;
                false
            }
        };

        let table_ref = TableReference::Full {
            catalog: Cow::from(self.database.clone()),
            schema: Cow::from(schema_name),
            table: Cow::from(table_name),
        };

        if !table_exists {
            self.create_delta_table(table_ref.clone(), plan.schema().as_ref())
                .await?;
            // TODO: This is really only needed here and for CREATE TABLE AS statements only to be
            // able to get the uuid without hitting the catalog DB in `get_table_uuid`
            self.reload_schema().await?;
        }

        self.plan_to_delta_table(table_ref, &plan).await?;

        Ok(())
    }
}

#[cfg(test)]
pub mod test_utils {
    use crate::config::context::build_context;
    use crate::config::schema;
    use crate::config::schema::{Catalog, SeafowlConfig, Sqlite};
    use sqlx::sqlite::SqliteJournalMode;

    use super::*;

    /// Build a real (not mocked) in-memory context that uses SQLite
    pub async fn in_memory_context() -> DefaultSeafowlContext {
        let config = SeafowlConfig {
            object_store: schema::ObjectStore::InMemory(schema::InMemory {}),
            catalog: Catalog::Sqlite(Sqlite {
                dsn: "sqlite://:memory:".to_string(),
                journal_mode: SqliteJournalMode::Wal,
                read_only: false,
            }),
            frontend: Default::default(),
            runtime: Default::default(),
            misc: Default::default(),
        };
        build_context(&config).await.unwrap()
    }

    pub async fn in_memory_context_with_test_db() -> Arc<DefaultSeafowlContext> {
        let context = in_memory_context().await;

        // Create new non-default database
        let plan = context.plan_query("CREATE DATABASE testdb").await.unwrap();
        context.collect(plan).await.unwrap();

        let context = context.scope_to_database("testdb".to_string()).unwrap();

        // Create new non-default collection
        let plan = context.plan_query("CREATE SCHEMA testcol").await.unwrap();
        context.collect(plan).await.unwrap();

        // Create table
        let plan = context
            .plan_query("CREATE TABLE testcol.some_table (date DATE, value DOUBLE)")
            .await
            .unwrap();
        context.collect(plan).await.unwrap();

        context
    }
}

#[cfg(test)]
mod tests {
    use crate::context::test_utils::in_memory_context_with_test_db;
    use datafusion::assert_batches_eq;
    use std::sync::Arc;

    use super::*;

    use super::test_utils::in_memory_context;

    #[tokio::test]
    async fn test_create_table_without_columns_fails() {
        let context = Arc::new(in_memory_context().await);
        let err = context
            .plan_query("CREATE TABLE test_table")
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("At least one column must be defined to create a table."));
    }

    #[tokio::test]
    async fn test_drop_table_pending_deletion() -> Result<()> {
        let context = Arc::new(in_memory_context().await);
        let plan = context
            .plan_query("CREATE TABLE test_table (\"key\" INTEGER, value STRING)")
            .await
            .unwrap();
        context.collect(plan).await.unwrap();
        let plan = context.plan_query("DROP TABLE test_table").await.unwrap();
        context.collect(plan).await.unwrap();

        let plan = context
            .plan_query("SELECT table_schema, table_name, uuid, deletion_status FROM system.dropped_tables")
            .await
            .unwrap();
        let results = context.collect(plan).await.unwrap();

        let expected = vec![
            "+--------------+------------+--------------------------------------+-----------------+",
            "| table_schema | table_name | uuid                                 | deletion_status |",
            "+--------------+------------+--------------------------------------+-----------------+",
            "| public       | test_table | 01020304-0506-4708-890a-0b0c0d0e0f10 | PENDING         |",
            "+--------------+------------+--------------------------------------+-----------------+",
        ];
        assert_batches_eq!(expected, &results);

        Ok(())
    }

    #[tokio::test]
    async fn test_plan_insert_normal() {
        let sf_context = in_memory_context_with_test_db().await;

        let plan = sf_context
            .create_logical_plan(
                "INSERT INTO testcol.some_table (date, value) VALUES('2022-01-01T12:00:00', 42)",
            )
            .await
            .unwrap();

        assert_eq!(
            format!("{plan:?}"),
            "Dml: op=[Insert] table=[testcol.some_table]\
            \n  Projection: CAST(column1 AS Date32) AS date, CAST(column2 AS Float64) AS value\
            \n    Values: (Utf8(\"2022-01-01T12:00:00\"), Int64(42))"
        );
    }

    #[tokio::test]
    async fn test_plan_insert_renaming() {
        let sf_context = in_memory_context_with_test_db().await;

        let plan = sf_context
            .create_logical_plan(
                "INSERT INTO testcol.some_table (date, value)
                SELECT \"date\" AS my_date, \"value\" AS my_value FROM testdb.testcol.some_table",
            )
            .await
            .unwrap();

        assert_eq!(format!("{plan:?}"), "Dml: op=[Insert] table=[testcol.some_table]\
        \n  Projection: testdb.testcol.some_table.date AS date, testdb.testcol.some_table.value AS value\
        \n    TableScan: testdb.testcol.some_table projection=[date, value]");
    }

    #[tokio::test]
    async fn test_execute_insert_from_other_table() -> Result<()> {
        let context = Arc::new(in_memory_context().await);
        context
            .collect(
                context
                    .plan_query(
                        "CREATE TABLE test_table (\"key\" INTEGER, value STRING);",
                    )
                    .await?,
            )
            .await?;

        context
            .collect(
                context
                    .plan_query("INSERT INTO test_table VALUES (1, 'one'), (2, 'two');")
                    .await?,
            )
            .await?;

        context
        .collect(
            context
                .plan_query("INSERT INTO test_table(key, value) SELECT * FROM test_table WHERE value = 'two'")
                .await?,
        )
        .await?;

        let results = context
            .collect(
                context
                    .plan_query("SELECT * FROM test_table ORDER BY key ASC")
                    .await?,
            )
            .await?;

        let expected = vec![
            "+-----+-------+",
            "| key | value |",
            "+-----+-------+",
            "| 1   | one   |",
            "| 2   | two   |",
            "| 2   | two   |",
            "+-----+-------+",
        ];
        assert_batches_eq!(expected, &results);

        Ok(())
    }

    #[tokio::test]
    async fn test_execute_insert_from_other_table_schema_qualifier() -> Result<()> {
        let context = Arc::new(in_memory_context().await);
        context
            .collect(
                context
                    .plan_query(
                        "CREATE TABLE test_table (\"key\" INTEGER, value STRING);",
                    )
                    .await?,
            )
            .await?;

        context
            .collect(
                context
                    .plan_query(
                        "INSERT INTO public.test_table VALUES (1, 'one'), (2, 'two');",
                    )
                    .await?,
            )
            .await?;

        context
        .collect(
            context
                .plan_query("INSERT INTO test_table(key, value) SELECT * FROM public.test_table WHERE value = 'two'")
                .await?,
        )
        .await?;

        let results = context
            .collect(
                context
                    .plan_query("SELECT * FROM test_table ORDER BY key ASC")
                    .await?,
            )
            .await?;

        let expected = vec![
            "+-----+-------+",
            "| key | value |",
            "+-----+-------+",
            "| 1   | one   |",
            "| 2   | two   |",
            "| 2   | two   |",
            "+-----+-------+",
        ];
        assert_batches_eq!(expected, &results);

        Ok(())
    }

    async fn get_logical_plan(query: &str) -> String {
        let sf_context = in_memory_context_with_test_db().await;

        let plan = sf_context.create_logical_plan(query).await.unwrap();
        format!("{plan:?}")
    }

    #[tokio::test]
    async fn test_plan_create_schema_name_in_quotes() {
        assert_eq!(
            get_logical_plan("CREATE SCHEMA schema_name;").await,
            "CreateCatalogSchema: \"schema_name\""
        );
        assert_eq!(
            get_logical_plan("CREATE SCHEMA \"schema_name\";").await,
            "CreateCatalogSchema: \"schema_name\""
        );
    }

    #[tokio::test]
    async fn test_plan_rename_table_name_in_quotes() {
        assert_eq!(
            get_logical_plan("ALTER TABLE \"testcol\".\"some_table\" RENAME TO \"testcol\".\"some_table_2\"").await,
            "RenameTable: testcol.some_table to testcol.some_table_2"
        );
    }

    #[tokio::test]
    async fn test_plan_drop_table_name_in_quotes() {
        assert_eq!(
            get_logical_plan("DROP TABLE \"testcol\".\"some_table\"").await,
            "DropTable: Partial { schema: \"testcol\", table: \"some_table\" } if not exist:=false"
        );
    }

    #[tokio::test]
    async fn test_plan_insert_all() {
        let sf_context = in_memory_context_with_test_db().await;

        let plan = sf_context
            .create_logical_plan(
                "INSERT INTO testcol.some_table VALUES('2022-01-01T12:00:00', 42)",
            )
            .await
            .unwrap();

        assert_eq!(
            format!("{plan:?}"),
            "Dml: op=[Insert] table=[testcol.some_table]\
            \n  Projection: CAST(column1 AS Date32) AS date, CAST(column2 AS Float64) AS value\
            \n    Values: (Utf8(\"2022-01-01T12:00:00\"), Int64(42))"
        );
    }

    #[tokio::test]
    async fn test_plan_insert_type_mismatch() {
        let sf_context = in_memory_context_with_test_db().await;

        // Try inserting a timestamp into a number (note this will work fine for inserting
        // e.g. Utf-8 into numbers at plan time but should fail at execution time if the value
        // doesn't convert)
        let err = sf_context
            .create_logical_plan("INSERT INTO testcol.some_table SELECT '2022-01-01', to_timestamp('2022-01-01T12:00:00')")
            .await.unwrap_err();
        assert_eq!(err.to_string(), "Error during planning: Cannot automatically convert Timestamp(Nanosecond, None) to Float64");
    }

    #[tokio::test]
    async fn test_plan_insert_values_wrong_number() {
        let sf_context = in_memory_context_with_test_db().await;

        let err = sf_context
            .create_logical_plan(
                "INSERT INTO testcol.some_table VALUES('2022-01-01T12:00:00')",
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Error during planning: Column count doesn't match insert query!"
        );
    }

    #[tokio::test]
    async fn test_plan_insert_values_duplicate_columns() {
        let sf_context = in_memory_context_with_test_db().await;

        let err = sf_context
            .create_logical_plan("INSERT INTO testcol.some_table(date, date, value) VALUES('2022-01-01T12:00:00', '2022-01-01T12:00:00', 42)")
            .await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Schema error: Schema contains duplicate unqualified field name 'date'"
        );
    }

    #[tokio::test]
    async fn test_register_udf() -> Result<()> {
        let sf_context = in_memory_context().await;

        // Source: https://gist.github.com/going-digital/02e46c44d89237c07bc99cd440ebfa43
        sf_context.collect(sf_context.plan_query(
            r#"CREATE FUNCTION sintau AS '
            {
                "entrypoint": "sintau",
                "language": "wasm",
                "input_types": ["float"],
                "return_type": "float",
                "data": "AGFzbQEAAAABDQJgAX0BfWADfX9/AX0DBQQAAAABBQQBAUREBxgDBnNpbnRhdQAABGV4cDIAAQRsb2cyAAIKjgEEKQECfUMAAAA/IgIgACAAjpMiACACk4siAZMgAZZBAEEYEAMgAiAAk5gLGQAgACAAjiIAk0EYQSwQA7wgAKhBF3RqvgslAQF/IAC8IgFBF3ZB/wBrsiABQQl0s0MAAIBPlUEsQcQAEAOSCyIBAX0DQCADIACUIAEqAgCSIQMgAUEEaiIBIAJrDQALIAMLC0oBAEEAC0Q/x2FC2eATQUuqKsJzsqY9QAHJQH6V0DZv+V88kPJTPSJndz6sZjE/HQCAP/clMD0D/T++F6bRPkzcNL/Tgrg//IiKNwBqBG5hbWUBHwQABnNpbnRhdQEEZXhwMgIEbG9nMgMIZXZhbHBvbHkCNwQAAwABeAECeDECBGhhbGYBAQABeAICAAF4AQJ4aQMEAAF4AQVzdGFydAIDZW5kAwZyZXN1bHQDCQEDAQAEbG9vcA=="
            }';"#,
        )
        .await?).await?;

        let results = sf_context
            .collect(
                sf_context
                    .plan_query(
                        "
        SELECT v, ROUND(sintau(CAST(v AS REAL)) * 100) AS sintau
        FROM (VALUES (0.1), (0.2), (0.3), (0.4), (0.5)) d (v)",
                    )
                    .await?,
            )
            .await?;

        let expected = vec![
            "+-----+--------+",
            "| v   | sintau |",
            "+-----+--------+",
            "| 0.1 | 59.0   |",
            "| 0.2 | 95.0   |",
            "| 0.3 | 95.0   |",
            "| 0.4 | 59.0   |",
            "| 0.5 | 0.0    |",
            "+-----+--------+",
        ];

        assert_batches_eq!(expected, &results);

        Ok(())
    }

    #[tokio::test]
    async fn test_register_invalid_udf() -> Result<()> {
        let sf_context = in_memory_context().await;

        // Source: https://gist.github.com/going-digital/02e46c44d89237c07bc99cd440ebfa43
        let plan = sf_context
            .plan_query(
                r#"CREATE FUNCTION invalidfn AS '
            {
                "entrypoint": "invalidfn",
                "language": "wasmMessagePack",
                "input_types": ["float"],
                "return_type": "float",
                "data": ""
            }';"#,
            )
            .await;
        assert!(plan.is_err());
        assert!(plan.err().unwrap().to_string().starts_with(
            "Internal error: Error initializing WASM + MessagePack UDF \"invalidfn\": Internal(\"Error loading WASM module: failed to parse WebAssembly module"));
        Ok(())
    }
}
