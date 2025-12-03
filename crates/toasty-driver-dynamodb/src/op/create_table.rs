use toasty_core::{schema::db::{ColumnId, IndexColumn, IndexScope}, stmt::Type};

use super::{
    ddb_key_schema, ddb_ty, AttributeDefinition, DynamoDb, GlobalSecondaryIndex, Projection,
    ProjectionType, ProvisionedThroughput, Result, Schema, Table,
};

impl DynamoDb {
    pub(crate) async fn create_table(
        &self,
        schema: &Schema,
        table: &Table,
        reset: bool,
    ) -> Result<()> {
        if reset {
            let _ = self
                .client
                .delete_table()
                .table_name(&table.name)
                .send()
                .await;

            for index in &table.indices {
                if !index.primary_key && index.unique {
                    let _ = self
                        .client
                        .delete_table()
                        .table_name(&index.name)
                        .send()
                        .await;
                }
            }
        }

        let pt = ProvisionedThroughput::builder()
            .read_capacity_units(10)
            .write_capacity_units(5)
            .build()
            .unwrap();

        // Calculate which attributes need to be defined
        let mut defined_attributes: std::collections::HashMap<String, Type> = std::collections::HashMap::new();

        let (partition_cols, sort_cols): (Vec<(&IndexColumn, ColumnId)>, Vec<(&IndexColumn, ColumnId)>) = table.indices[table.primary_key.index.index].columns.iter().map(|c| (c, c.column)).partition(|(c, _)| matches!(c.scope, IndexScope::Partition));

        assert!(partition_cols.len() == 1);
        let partition_column_id = partition_cols.first().unwrap().1;
        let partition_column = table.column(partition_column_id);
        defined_attributes.insert(partition_column.name.clone(), partition_column.ty.clone());
        let mut range_column: Option<String> = None;
        if sort_cols.len() > 1 {
            // need to concatenate these into a single attribute, here we just need to define the attribute
            let name: String = "__sk".into();
            range_column = Some(name.clone());
            defined_attributes.insert(name, Type::String);
        } else if sort_cols.len() == 1 {
            let sort_col = table.column(sort_cols.first().unwrap().1);
            range_column = Some(sort_col.name.clone());
            defined_attributes.insert(sort_col.name.clone(), sort_col.ty.clone());
        }

        let mut gsis = vec![];

        for index in &table.indices {
            if index.primary_key || index.unique {
                continue;
            }

            assert_eq!(1, index.columns.len());
            let field = &table.column(index.columns[0].column);
            defined_attributes.insert(field.name.clone(), field.ty.clone());

            gsis.push(
                GlobalSecondaryIndex::builder()
                    .index_name(&index.name)
                    .set_key_schema(Some(ddb_key_schema(&field.name, None)))
                    .projection(
                        Projection::builder()
                            .projection_type(ProjectionType::All)
                            .build(),
                    )
                    .provisioned_throughput(pt.clone())
                    .build()
                    .unwrap(),
            );
        }

        let attribute_definitions = defined_attributes
            .iter()
            .map(|(name, ty)| {
                AttributeDefinition::builder()
                    .attribute_name(name)
                    .attribute_type(ddb_ty(ty))
                    .build()
                    .unwrap()
            })
            .collect();

        self.client
            .create_table()
            .table_name(&table.name)
            .set_attribute_definitions(Some(attribute_definitions))
            .set_key_schema(Some(ddb_key_schema(&partition_column.name, range_column.as_ref())))
            .set_global_secondary_indexes(if gsis.is_empty() { None } else { Some(gsis) })
            .provisioned_throughput(pt.clone())
            .send()
            .await?;

        // Now, create separate tables for each unique index
        for index in table.indices.iter().filter(|i| !i.primary_key && i.unique) {
            // TODO: handle more than one column
            assert_eq!(1, index.columns.len());

            let pk = schema.column(index.columns[0].column);

            self.client
                .create_table()
                .table_name(&index.name)
                .set_key_schema(Some(ddb_key_schema(&pk.name, None)))
                .attribute_definitions(
                    AttributeDefinition::builder()
                        .attribute_name(&pk.name)
                        .attribute_type(ddb_ty(&pk.ty))
                        .build()
                        .unwrap(),
                )
                .provisioned_throughput(pt.clone())
                .send()
                .await?;
        }

        Ok(())
    }
}
