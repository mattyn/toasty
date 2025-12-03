use crate::sort_key_columns;

use super::{
    ddb_val, stmt, DynamoDb, Put, PutRequest, Result, Schema, TransactWriteItem, WriteRequest,
};
use std::collections::{HashMap, HashSet};
use aws_sdk_dynamodb::types::AttributeValue;
use toasty_core::{driver::Response, schema::db::ColumnId, stmt::Value};

impl DynamoDb {
    pub(crate) async fn exec_insert(
        &self,
        schema: &Schema,
        insert: stmt::Insert,
    ) -> Result<Response> {
        assert!(insert.returning.is_none());

        let insert_table = insert.target.as_table_unwrap();
        let table = &schema.table(insert_table.table);

        let unique_indices = table
            .indices
            .iter()
            .filter(|index| {
                if !index.primary_key && index.unique {
                    // Don't update the index if the value is not included.
                    index.columns.iter().all(|index_column| {
                        let column = schema.column(index_column.column);
                        insert_table.columns.contains(&column.id)
                    })
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();

        // Create the item map
        let mut insert_items = vec![];

        let source = insert.source.body.into_values();

        let sk_cols: Vec<ColumnId> = sort_key_columns(table);
        let concat_sks = sk_cols.len() > 1;
        let sk_col_ids: HashSet<ColumnId> = sk_cols.iter().map(|c| c.clone()).collect();

        for row in source.rows {
            let mut items = HashMap::new();
            let mut sk_vals: HashMap<ColumnId, Value> = HashMap::new();

            for (i, column_id) in insert_table.columns.iter().enumerate() {
                let column = schema.column(*column_id);
                let entry = row.entry(i);
                let value = entry.as_value();

                if concat_sks && sk_col_ids.contains(column_id) {
                    // collect the value, we will add this later
                    sk_vals.insert(column_id.clone(), value.clone());
                    continue;
                }

                if !value.is_null() {
                    items.insert(column.name.clone(), ddb_val(&value));
                }
            }

            if concat_sks {
                let mut sk_val = sk_cols.iter()
                    .map(|c| sk_vals.get(&c).expect("SK value not provided for insert"))
                    .map(|v| v.as_str().map(|s| String::from(s)))
                    .filter(|v| v.is_some())
                    .map(|o| o.unwrap())
                    .collect::<Vec<String>>()
                    .join("#");
                sk_val.push('#');
                items.insert("__sk".into(), AttributeValue::S(sk_val));
            }
            insert_items.push(items);
        }

        let count = insert_items.len();

        match &unique_indices[..] {
            [] => {
                if insert_items.len() == 1 {
                    let insert_items = insert_items.into_iter().next().unwrap();

                    self.client
                        .put_item()
                        .table_name(&table.name)
                        .set_item(Some(insert_items))
                        .send()
                        .await?;
                } else {
                    let mut request_items = HashMap::new();
                    request_items.insert(
                        table.name.clone(),
                        insert_items
                            .into_iter()
                            .map(|insert_item| {
                                WriteRequest::builder()
                                    .put_request(
                                        PutRequest::builder()
                                            .set_item(Some(insert_item))
                                            .build()
                                            .unwrap(),
                                    )
                                    .build()
                            })
                            .collect(),
                    );

                    self.client
                        .batch_write_item()
                        .set_request_items(Some(request_items))
                        .send()
                        .await?;
                }
            }
            [index] => {
                let mut transact_items = vec![];

                for insert_items in insert_items {
                    let mut index_insert_items = HashMap::new();
                    let mut expression_names = HashMap::new();
                    let mut condition_expression = String::new();
                    let mut nullable = false;

                    for index_column in &index.columns {
                        let column = schema.column(index_column.column);

                        if !insert_items.contains_key(&column.name) {
                            nullable = true;
                            break;
                        }

                        index_insert_items
                            .insert(column.name.clone(), insert_items[&column.name].clone());

                        if condition_expression.is_empty() {
                            let name = format!("#{}", column.id.index);
                            condition_expression = format!("attribute_not_exists({name})");
                            expression_names.insert(name, column.name.clone());
                        }
                    }

                    if !nullable {
                        // Add primary key values
                        for column in table.primary_key_columns() {
                            let name = &column.name;
                            index_insert_items.insert(name.clone(), insert_items[name].clone());
                        }

                        transact_items.push(
                            TransactWriteItem::builder()
                                .put(
                                    Put::builder()
                                        .table_name(&index.name)
                                        .set_item(Some(index_insert_items))
                                        .condition_expression(condition_expression)
                                        .set_expression_attribute_names(Some(expression_names))
                                        .build()
                                        .unwrap(),
                                )
                                .build(),
                        );
                    }

                    transact_items.push(
                        TransactWriteItem::builder()
                            .put(
                                Put::builder()
                                    .table_name(&table.name)
                                    .set_item(Some(insert_items))
                                    .build()
                                    .unwrap(),
                            )
                            .build(),
                    );
                }

                self.client
                    .transact_write_items()
                    .set_transact_items(Some(transact_items))
                    .send()
                    .await?;
            }
            _ => todo!(),
        }

        Ok(Response::count(count as _))
    }
}
