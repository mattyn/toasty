use crate::{BuildKeyExpression, sort_key_columns};

use super::{ddb_expression, item_to_record, operation, stmt, DynamoDb, ExprAttrs, Result, Schema};
use std::{collections::HashMap, sync::Arc};
use toasty_core::{driver::Response, schema::db::{Column, ColumnId, IndexScope}, stmt::ExprContext};

impl DynamoDb {
    pub(crate) async fn exec_query_pk(
        &self,
        schema: &Arc<Schema>,
        op: operation::QueryPk,
    ) -> Result<Response> {
        let table = schema.table(op.table);
        let cx = ExprContext::new_with_target(&**schema, table);

        let mut expr_attrs = ExprAttrs::default();
        let sk_cols: Vec<ColumnId> = sort_key_columns(table);
        let pk_cols: Vec<&Column> = table.indices[table.primary_key.index.index].columns.iter().filter(|c| matches!(c.scope, IndexScope::Partition)).map(|c| table.column(c.column)).collect();
        assert!(pk_cols.len() == 1);
        let concat_sks = sk_cols.len() > 1;
        let key_expression = BuildKeyExpression {
            cx: &cx,
            attrs: &mut expr_attrs,
            expr: &op.pk_filter,
            concat_sk: concat_sks,
            pk_column: pk_cols.first().as_ref().unwrap(),
            sk_columns: &sk_cols,
            pk_component: None,
            sk_components: HashMap::new(),
        }.build();

        let filter_expression = op
            .filter
            .as_ref()
            .map(|expr| ddb_expression(&cx, &mut expr_attrs, false, expr));

        let res = self
            .client
            .query()
            .table_name(&table.name)
            .key_condition_expression(key_expression)
            .set_filter_expression(filter_expression)
            .set_expression_attribute_names(Some(expr_attrs.attr_names))
            .set_expression_attribute_values(Some(expr_attrs.attr_values))
            .send()
            .await?;

        let schema = schema.clone();

        Ok(Response::value_stream(stmt::ValueStream::from_iter(
            res.items.into_iter().flatten().map(move |item| {
                item_to_record(
                    &item,
                    op.select.iter().map(|column_id| schema.column(*column_id)),
                    &sk_cols
                )
            }),
        )))
    }
}
