use std::{any::Any, collections::HashMap, sync::Arc};

use async_trait::async_trait;
use datafusion::{
    arrow::datatypes::SchemaRef as ArrowSchemaRef,
    catalog::{catalog::CatalogProvider, schema::SchemaProvider},
    common::{DataFusionError, Result},
    datasource::TableProvider,
    execution::context::{SessionState, TaskContext},
    logical_expr::TableType,
    logical_plan::Expr,
    physical_expr::PhysicalSortExpr,
    physical_plan::{ExecutionPlan, Partitioning, SendableRecordBatchStream, Statistics},
};

pub struct SeafowlDatabase {
    pub name: Arc<str>,
    pub collections: HashMap<Arc<str>, Arc<SeafowlCollection>>,
}

impl CatalogProvider for SeafowlDatabase {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema_names(&self) -> Vec<String> {
        self.collections.keys().map(|s| s.to_string()).collect()
    }

    fn schema(&self, name: &str) -> Option<Arc<dyn SchemaProvider>> {
        self.collections.get(name).map(|c| Arc::clone(c) as _)
    }
}

pub struct SeafowlCollection {
    pub name: Arc<str>,
    pub tables: HashMap<Arc<str>, Arc<SeafowlTable>>,
}

impl SchemaProvider for SeafowlCollection {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn table_names(&self) -> Vec<String> {
        self.tables.keys().map(|s| s.to_string()).collect()
    }

    fn table(&self, name: &str) -> Option<Arc<dyn TableProvider>> {
        self.tables.get(name).map(|c| Arc::clone(c) as _)
    }

    fn table_exist(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }
}

pub struct SeafowlRegion {
    pub object_storage_id: Arc<str>,
    pub row_count: i32,
    pub columns: Arc<Vec<RegionColumn>>,
}

pub struct RegionColumn {
    pub name: Arc<str>,
    pub r#type: Arc<str>,
    pub min_value: Arc<Option<Vec<u8>>>,
    pub max_value: Arc<Option<Vec<u8>>>,
}

pub struct Schema {
    pub arrow_schema: ArrowSchemaRef,
}

impl Schema {
    pub fn from_column_names_types<'a, I>(_columns: I) -> Self
    where
        I: Iterator<Item = (&'a String, &'a String)>,
    {
        todo!()
    }
}

pub struct SeafowlTable {
    pub name: Arc<str>,
    pub schema: Arc<Schema>,
    pub regions: Arc<Vec<SeafowlRegion>>,
}

#[async_trait]
impl TableProvider for SeafowlTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> ArrowSchemaRef {
        self.schema.arrow_schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _ctx: &SessionState,
        _projection: &Option<Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> std::result::Result<Arc<dyn ExecutionPlan>, DataFusionError> {
        // Filter partitions by the predicate
        // Create the node to scan through them
        // No UNION node here?
        Ok(Arc::new(SeafowlBaseTableScanNode {}))
    }
}

#[derive(Debug)]
struct SeafowlBaseTableScanNode {
    // TODO: list of partitions to scan through
}

impl ExecutionPlan for SeafowlBaseTableScanNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> ArrowSchemaRef {
        todo!()
    }

    fn output_partitioning(&self) -> Partitioning {
        todo!()
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        None
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        todo!()
        // Hit the object store up for a certain partition, scan through it
    }

    fn statistics(&self) -> Statistics {
        Statistics::default()
    }
}
