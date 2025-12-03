mod op;

use toasty_core::{
    driver::{Capability, Driver, Response, operation::Operation},
    schema::{
        app,
        db::{Column, ColumnId, IndexScope, Schema, Table},
    },
    stmt::{self, Expr, ExprContext, Visit},
};

use anyhow::Result;
use aws_sdk_dynamodb::{
    error::SdkError,
    operation::update_item::UpdateItemError,
    types::{
        AttributeDefinition, AttributeValue, Delete, GlobalSecondaryIndex, KeySchemaElement,
        KeyType, KeysAndAttributes, Projection, ProjectionType, ProvisionedThroughput, Put,
        PutRequest, ReturnValuesOnConditionCheckFailure, ScalarAttributeType, TransactWriteItem,
        Update, WriteRequest,
    },
    Client,
};
use std::{collections::HashMap, sync::Arc};
use url::Url;

#[derive(Debug)]
pub struct DynamoDb {
    /// Handle to the AWS SDK client
    client: Client,
}

impl DynamoDb {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub async fn connect(url: &str) -> Result<Self> {
        let url = Url::parse(url)?;

        if url.scheme() != "dynamodb" {
            return Err(anyhow::anyhow!(
                "connection URL does not have a `dynamodb` scheme; url={url}"
            ));
        }

        use aws_config::BehaviorVersion;
        use aws_sdk_dynamodb::config::Credentials;

        let mut aws_config = aws_config::defaults(BehaviorVersion::latest())
            .region("us-east-1")
            .credentials_provider(Credentials::for_tests());

        if let Some(host) = url.host() {
            let mut endpoint_url = format!("http://{host}");

            if let Some(port) = url.port() {
                endpoint_url.push_str(&format!(":{port}"));
            }

            aws_config = aws_config.endpoint_url(&endpoint_url);
        }

        let sdk_config = aws_config.load().await;

        let client = Client::new(&sdk_config);

        Ok(Self { client })
    }

    pub async fn from_env() -> Result<Self> {
        use aws_config::BehaviorVersion;
        use aws_sdk_dynamodb::config::Credentials;

        let sdk_config = aws_config::defaults(BehaviorVersion::latest())
            .region("foo")
            .credentials_provider(Credentials::for_tests())
            .endpoint_url("http://localhost:8000")
            .load()
            .await;

        let client = Client::new(&sdk_config);

        Ok(Self { client })
    }
}

#[toasty_core::async_trait]
impl Driver for DynamoDb {
    fn capability(&self) -> &Capability {
        &Capability::DYNAMODB
    }

    async fn register_schema(&mut self, _schema: &Schema) -> Result<()> {
        Ok(())
    }

    async fn exec(&self, schema: &Arc<Schema>, op: Operation) -> Result<Response> {
        self.exec2(schema, op).await
    }

    async fn reset_db(&self, schema: &Schema) -> Result<()> {
        for table in &schema.tables {
            self.create_table(schema, table, true).await?;
        }

        Ok(())
    }
}

impl DynamoDb {
    async fn exec2(&self, schema: &Arc<Schema>, op: Operation) -> Result<Response> {
        use Operation::*;

        match op {
            GetByKey(op) => self.exec_get_by_key(schema, op).await,
            QueryPk(op) => self.exec_query_pk(schema, op).await,
            DeleteByKey(op) => self.exec_delete_by_key(schema, op).await,
            UpdateByKey(op) => self.exec_update_by_key(schema, op).await,
            FindPkByIndex(op) => self.exec_find_pk_by_index(schema, op).await,
            QuerySql(op) => match op.stmt {
                stmt::Statement::Insert(op) => self.exec_insert(schema, op).await,
                _ => todo!("op={:#?}", op),
            },
            _ => todo!("op={op:#?}"),
        }
    }
}

fn ddb_ty(ty: &stmt::Type) -> ScalarAttributeType {
    use stmt::Type::*;
    use ScalarAttributeType::*;

    match ty {
        Bool => N,
        String | Enum(..) => S,
        I8 | I16 | I32 | I64 => N,
        Id(_) => S,
        _ => todo!("ddb_ty; ty={:#?}", ty),
    }
}

fn ddb_key(table: &Table, key: &stmt::Value) -> HashMap<String, AttributeValue> {
    let mut ret = HashMap::new();

    let mut sk_values: Vec<(&String, &stmt::Value)> = Vec::new();
    for (index, index_column) in table.indices[table.primary_key.index.index].columns.iter().enumerate() {
        let column = table.column(index_column.column);
        let value = match key {
            stmt::Value::Record(record) => &record[index],
            value => value,
        };

        match index_column.scope {
            IndexScope::Local => {
                sk_values.push((&column.name, value));
            },
            IndexScope::Partition => {
                ret.insert(column.name.clone(), ddb_val(value));
            }
        }
    }

    if sk_values.len() > 1 {
        // we need to concat them
        let mut sk_value: Vec<String> = Vec::new();
        for (_name, val) in sk_values {
            let Some(val_string) = val.as_str() else {
                continue;
            };
            sk_value.push(String::from(val_string));
        }
        let mut sk_value = sk_value.join("#");
        sk_value.push('#');
        ret.insert(String::from("__sk"), AttributeValue::S(sk_value));
    } else if sk_values.len() == 1 {
        let (name, val) = *sk_values.first().unwrap();
        ret.insert(name.clone(), ddb_val(val));
    }

    ret
}

#[derive(serde::Serialize, serde::Deserialize)]
enum V {
    Bool(bool),
    Null,
    String(String),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    Id(usize, String),
}

fn ddb_val(val: &stmt::Value) -> AttributeValue {
    match val {
        stmt::Value::Bool(val) => AttributeValue::Bool(*val),
        stmt::Value::String(val) => AttributeValue::S(val.to_string()),
        stmt::Value::I8(val) => AttributeValue::N(val.to_string()),
        stmt::Value::I16(val) => AttributeValue::N(val.to_string()),
        stmt::Value::I32(val) => AttributeValue::N(val.to_string()),
        stmt::Value::I64(val) => AttributeValue::N(val.to_string()),
        stmt::Value::U8(val) => AttributeValue::N(val.to_string()),
        stmt::Value::U16(val) => AttributeValue::N(val.to_string()),
        stmt::Value::U32(val) => AttributeValue::N(val.to_string()),
        stmt::Value::U64(val) => AttributeValue::N(val.to_string()),
        stmt::Value::Bytes(val) => AttributeValue::B(val.clone().into()),
        stmt::Value::Uuid(val) => AttributeValue::S(val.to_string()),
        stmt::Value::Id(val) => AttributeValue::S(val.to_string()),
        stmt::Value::Enum(val) => {
            let v = match &val.fields[..] {
                [] => V::Null,
                [stmt::Value::Bool(v)] => V::Bool(*v),
                [stmt::Value::String(v)] => V::String(v.to_string()),
                [stmt::Value::I8(v)] => V::I8(*v),
                [stmt::Value::I16(v)] => V::I16(*v),
                [stmt::Value::I32(v)] => V::I32(*v),
                [stmt::Value::I64(v)] => V::I64(*v),
                [stmt::Value::U8(v)] => V::U8(*v),
                [stmt::Value::U16(v)] => V::U16(*v),
                [stmt::Value::U32(v)] => V::U32(*v),
                [stmt::Value::U64(v)] => V::U64(*v),
                [stmt::Value::Id(id)] => V::Id(id.model_id().0, id.to_string()),
                _ => todo!("val={:#?}", val.fields),
            };
            AttributeValue::S(format!(
                "{}#{}",
                val.variant,
                serde_json::to_string(&v).unwrap()
            ))
        }
        _ => todo!("{:#?}", val),
    }
}

fn ddb_to_val(ty: &stmt::Type, val: &AttributeValue) -> stmt::Value {
    use stmt::Type;
    use AttributeValue::*;

    match (ty, val) {
        (Type::Bool, Bool(val)) => stmt::Value::from(*val),
        (Type::String, S(val)) => stmt::Value::from(val.clone()),
        (Type::I8, N(val)) => stmt::Value::from(val.parse::<i8>().unwrap()),
        (Type::I16, N(val)) => stmt::Value::from(val.parse::<i16>().unwrap()),
        (Type::I32, N(val)) => stmt::Value::from(val.parse::<i32>().unwrap()),
        (Type::I64, N(val)) => stmt::Value::from(val.parse::<i64>().unwrap()),
        (Type::U8, N(val)) => stmt::Value::from(val.parse::<u8>().unwrap()),
        (Type::U16, N(val)) => stmt::Value::from(val.parse::<u16>().unwrap()),
        (Type::U32, N(val)) => stmt::Value::from(val.parse::<u32>().unwrap()),
        (Type::U64, N(val)) => stmt::Value::from(val.parse::<u64>().unwrap()),
        (Type::Bytes, B(val)) => stmt::Value::Bytes(val.clone().into_inner()),
        (Type::Uuid, S(val)) => stmt::Value::from(val.parse::<uuid::Uuid>().unwrap()),
        (Type::Id(model), S(val)) => stmt::Value::from(stmt::Id::from_string(*model, val.clone())),
        (Type::Enum(..), S(val)) => {
            let (variant, rest) = val.split_once("#").unwrap();
            let variant: usize = variant.parse().unwrap();
            let v: V = serde_json::from_str(rest).unwrap();
            let value = match v {
                V::Bool(v) => stmt::Value::Bool(v),
                V::Null => stmt::Value::Null,
                V::String(v) => stmt::Value::String(v),
                V::Id(model, v) => stmt::Value::Id(stmt::Id::from_string(app::ModelId(model), v)),
                V::I8(v) => stmt::Value::I8(v),
                V::I16(v) => stmt::Value::I16(v),
                V::I32(v) => stmt::Value::I32(v),
                V::I64(v) => stmt::Value::I64(v),
                V::U8(v) => stmt::Value::U8(v),
                V::U16(v) => stmt::Value::U16(v),
                V::U32(v) => stmt::Value::U32(v),
                V::U64(v) => stmt::Value::U64(v),
            };

            if value.is_null() {
                stmt::ValueEnum {
                    variant,
                    fields: stmt::ValueRecord::from_vec(vec![]),
                }
                .into()
            } else {
                stmt::ValueEnum {
                    variant,
                    fields: stmt::ValueRecord::from_vec(vec![value]),
                }
                .into()
            }
        }
        _ => todo!("ty={:#?}; value={:#?}", ty, val),
    }
}

fn ddb_key_schema(partition: &String, range: Option<&String>) -> Vec<KeySchemaElement> {
    let mut ks = vec![];

    ks.push(
        KeySchemaElement::builder()
            .attribute_name(partition.clone())
            .key_type(KeyType::Hash)
            .build()
            .unwrap(),
    );

    if let Some(range) = range {
        ks.push(
            KeySchemaElement::builder()
                .attribute_name(range.clone())
                .key_type(KeyType::Range)
                .build()
                .unwrap(),
        );
    }

    ks
}

fn item_to_record<'a, 'stmt>(
    item: &HashMap<String, AttributeValue>,
    columns: impl Iterator<Item = &'a Column>,
    sk_cols: &Vec<ColumnId>,
) -> Result<stmt::ValueRecord> {
    let mut sk_vals: HashMap<ColumnId, stmt::Value> = HashMap::new();
    if let Some(sk_val) = item.get("__sk") {
        let mut parts: Vec<&str> = sk_val.as_s().unwrap().split('#').collect();
        // we write a trailing delimeter so that begins with will work with both full and partial values
        parts.pop();
        assert!(parts.len() <= sk_cols.len(), "too many sort key values");
        for (index, part) in parts.iter().enumerate() {
            sk_vals.insert(sk_cols[index].clone(), stmt::Value::String(String::from(*part)));
        }
    }
    Ok(stmt::ValueRecord::from_vec(
        columns
            .map(|column| {
                if let Some(value) = item.get(&column.name) {
                    ddb_to_val(&column.ty, value)
                } else if let Some(value) = sk_vals.get(&column.id) {
                    value.clone()
                } else {
                    stmt::Value::Null
                }
            })
            .collect(),
    ))
}

fn sort_key_columns(table: &Table) -> Vec<ColumnId> {
    table.indices[table.primary_key.index.index].columns.iter()
        .filter(|c| matches!(c.scope, IndexScope::Local))
        .map(|c| table.column(c.column).id.clone())
        .collect()
}

struct BuildKeyExpression<'a, 'b> {
    cx: &'a ExprContext<'b, Schema>,
    attrs: &'a mut ExprAttrs,
    expr: &'a stmt::Expr,
    pk_column: &'a Column,
    sk_columns: &'a Vec<ColumnId>,
    concat_sk: bool,

    sk_components: HashMap<ColumnId, stmt::Expr>,
    pk_component: Option<stmt::Expr>,
}

impl <'a, 'b> BuildKeyExpression<'a, 'b> {
    fn build(&mut self) -> String {
        if !self.concat_sk {
            return ddb_expression(self.cx, self.attrs, true, self.expr);
        }

        // collect the subexpressions that test the sort keys and partition keys
        self.visit_expr(self.expr);

        let mut key_expr = String::new();
        // translate the PK subexpression unchanged
        let pk_expr = self.pk_component.as_ref().expect("key expression needs a hash key condition");
        key_expr.push_str(&ddb_expression(self.cx, self.attrs, true, &pk_expr));

        // add the SK subexpressions as a begins_with
        let mut missing_col = false;
        let mut sk_prefix = String::new();
        for sk_col in self.sk_columns {
            let Some(sk_expr) = self.sk_components.get(sk_col) else {
                missing_col = true;
                continue;
            };

            assert!(!missing_col, "gap in range key component conditions");

            match sk_expr {
                stmt::Expr::IsNull(_) => {
                    // skip adding, this column is not relevant to the model
                },
                stmt::Expr::BinaryOp(op) if matches!(op.op, stmt::BinaryOp::Eq) => {
                    let stmt::Expr::Value(val) = op.rhs.as_ref() else {
                        todo!("op={op:#?}");
                    };
                    sk_prefix.push_str(val.expect_string());
                    sk_prefix.push('#');
                },
                _ => todo!("sk_expr={sk_expr:#?}")
            }
        }
        let sk_prefix = self.attrs.literal(sk_prefix);
        
        self.attrs.attr_names.insert(String::from("#sk_col"), String::from("__sk"));
        format!("{key_expr} AND begins_with(#sk_col, {sk_prefix})")
    }
}

impl <'a, 'b> Visit for BuildKeyExpression<'a, 'b> {

    fn visit_expr(&mut self, i: &Expr) {
        match i {
            stmt::Expr::And(and) => self.visit_expr_and(and),
            stmt::Expr::BinaryOp(binop) => self.visit_expr_binary_op(binop),
            stmt::Expr::Value(val) => self.visit_value(val),
            stmt::Expr::Reference(refer) => self.visit_expr_reference(refer),
            stmt::Expr::IsNull(isnull) => self.visit_expr_is_null(isnull),
            _ => todo!("i={i:#?}")
        }
    }

    fn visit_expr_binary_op(&mut self, i: &stmt::ExprBinaryOp) {
        let stmt::Expr::Reference(refer) = i.lhs.as_ref() else {
            todo!("op={i:#?}");
        };
        assert!(matches!(i.op, stmt::BinaryOp::Eq), "unsupported condition {i:#?}");
        let column = self.cx.resolve_expr_reference(refer).expect_column();
        if self.pk_column.id == column.id {
            self.pk_component = Some(i.clone().into());
        } else {
            self.sk_components.insert(column.id.clone(), i.clone().into());
        }
    }

}

fn ddb_expression(
    cx: &ExprContext<'_, Schema>,
    attrs: &mut ExprAttrs,
    primary: bool,
    expr: &stmt::Expr,
) -> String {
    match expr {
        stmt::Expr::BinaryOp(expr_binary_op) => {
            let lhs = ddb_expression(cx, attrs, primary, &expr_binary_op.lhs);
            let rhs = ddb_expression(cx, attrs, primary, &expr_binary_op.rhs);

            match expr_binary_op.op {
                stmt::BinaryOp::Eq => format!("{lhs} = {rhs}"),
                stmt::BinaryOp::Ne if primary => {
                    todo!("!= conditions on primary key not supported")
                }
                stmt::BinaryOp::Ne => format!("{lhs} <> {rhs}"),
                stmt::BinaryOp::Gt => format!("{lhs} > {rhs}"),
                stmt::BinaryOp::Ge => format!("{lhs} >= {rhs}"),
                stmt::BinaryOp::Lt => format!("{lhs} < {rhs}"),
                stmt::BinaryOp::Le => format!("{lhs} <= {rhs}"),
                _ => todo!("OP {:?}", expr_binary_op.op),
            }
        }
        stmt::Expr::Reference(expr_reference) => {
            let column = cx.resolve_expr_reference(expr_reference).expect_column();
            attrs.column(column).to_string()
        }
        stmt::Expr::Value(val) => attrs.value(val),
        stmt::Expr::And(expr_and) => {
            let operands = expr_and
                .operands
                .iter()
                .map(|operand| ddb_expression(cx, attrs, primary, operand))
                .collect::<Vec<_>>();
            operands.join(" AND ")
        }
        stmt::Expr::Pattern(stmt::ExprPattern::BeginsWith(begins_with)) => {
            let expr = ddb_expression(cx, attrs, primary, &begins_with.expr);
            let substr = ddb_expression(cx, attrs, primary, &begins_with.pattern);
            format!("begins_with({expr}, {substr})")
        }
        _ => todo!("FILTER = {:#?}", expr),
    }
}

#[derive(Default, Debug)]
struct ExprAttrs {
    columns: HashMap<ColumnId, String>,
    attr_names: HashMap<String, String>,
    attr_values: HashMap<String, AttributeValue>,
}

impl ExprAttrs {
    fn column(&mut self, column: &Column) -> &str {
        use std::collections::hash_map::Entry;

        match self.columns.entry(column.id) {
            Entry::Vacant(e) => {
                let name = format!("#col_{}", column.id.index);
                self.attr_names.insert(name.clone(), column.name.clone());
                e.insert(name)
            }
            Entry::Occupied(e) => e.into_mut(),
        }
    }

    fn literal<T: Into<String>>(&mut self, val: T) -> String {
        self.ddb_value(AttributeValue::S(val.into()))
    }

    fn value(&mut self, val: &stmt::Value) -> String {
        self.ddb_value(ddb_val(val))
    }

    fn ddb_value(&mut self, val: AttributeValue) -> String {
        let i = self.attr_values.len();
        let name = format!(":v_{i}");
        self.attr_values.insert(name.clone(), val);
        name
    }
}
