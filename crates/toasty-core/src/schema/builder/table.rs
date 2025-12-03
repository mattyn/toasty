use super::BuildSchema;
use crate::{
    driver,
    schema::{
        app::{self, FieldId, FieldTy, Model, ModelId},
        db::{self, ColumnId, IndexColumn, IndexId, Table, TableId},
        mapping::{self, Mapping, TableToModel},
        Name,
    },
    stmt::{self},
};

struct BuildTableFromModels<'a> {
    /// Database-specific capabilities
    db: &'a driver::Capability,

    /// The table being built from the set of models
    table: &'a mut Table,

    /// Schema mapping
    mapping: &'a mut Mapping,

    /// When true, column names should be prefixed with their associated model
    /// names
    prefix_table_names: bool,
}

/// Computes a model's maping
struct BuildMapping<'a> {
    table: &'a mut Table,
    mapping: &'a mut mapping::Model,
    lowering_columns: Vec<ColumnId>,
    model_to_table: Vec<stmt::Expr>,
    model_pk_to_table: Vec<stmt::Expr>,
    table_to_model: Vec<stmt::Expr>,
}

impl BuildSchema<'_> {
    pub(super) fn build_table_stub_for_model(&mut self, model: &Model) -> TableId {
        if let Some(table_name) = &model.table_name {
            let table_name = self.prefix_table_name(table_name);

            if !self.table_lookup.contains_key(&table_name) {
                let id = self.register_table(&table_name);
                self.tables.push(Table::new(id, table_name.clone()));
            }

            *self.table_lookup.get(&table_name).unwrap()
        } else if let Some(_) = &model.item_collection {
            // return placeholder, so this can be fixed up in a second pass
            TableId::placeholder()
        } else {
            let name = self.table_name_from_model(&model.name);
            let id = self.register_table(&name);

            self.tables.push(Table::new(id, name));
            id
        }
    }

    pub(super) fn populate_item_collection_mapping(
        &mut self,
        app: &app::Schema,
        model: &Model,
    ) -> crate::Result<()> {
        if model.item_collection.is_some() {
            let (table, path) = self.find_item_collection_path(app, model);

            // ensure all paths up to the root are populated
            for (idx, mid) in path.iter().enumerate() {
                let source_model = self.mapping.model_mut(mid);
                source_model.table = table;

                if !source_model.item_collection.path.is_empty() {
                    source_model
                        .item_collection
                        .path
                        .extend_from_slice(&path[..(idx + 1)]);
                    source_model.item_collection.path.push(model.id.clone());
                }
            }

            // scan relations to map PK fields from parents to FK fields in this model
            let source_model = self.mapping.model_mut(model.id);
            for field in &model.fields {
                match &field.ty {
                    FieldTy::BelongsTo(rel) => {
                        for fk in &rel.foreign_key.fields {
                            if model.field(fk.source).primary_key {
                                source_model
                                    .item_collection
                                    .field_mapping
                                    .insert(fk.source.clone(), fk.target.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }

            Ok(())
        } else {
            Ok(())
        }
    }

    fn find_item_collection_path(
        &self,
        app: &app::Schema,
        model: &Model,
    ) -> (TableId, Vec<ModelId>) {
        let mut path: Vec<ModelId> = Vec::new();
        let table = self.find_item_collection_path_helper(app, model, &mut path);
        path.reverse();
        (table, path)
    }

    fn find_item_collection_path_helper(
        &self,
        app: &app::Schema,
        model: &Model,
        path: &mut Vec<ModelId>,
    ) -> TableId {
        if let Some(item_collection) = &model.item_collection {
            path.push(model.id.clone());
            self.find_item_collection_path_helper(app, app.model(item_collection), path)
        } else {
            path.push(model.id.clone());
            self.mapping.model(model.id).table
        }
    }

    pub(super) fn build_tables_from_models(&mut self, app: &app::Schema, db: &driver::Capability) {
        for table in &mut self.tables {
            let models = app
                .models()
                .filter(|model| self.mapping.model(model.id).table == table.id)
                .collect::<Vec<_>>();

            let (roots, children): (Vec<&Model>, Vec<&Model>) =
                models.iter().partition(|m| m.item_collection.is_none());

            assert!(roots.len() == 1, "item collection may only have one root");

            let root = roots[0];

            BuildTableFromModels {
                db,
                table,
                mapping: &mut self.mapping,
                prefix_table_names: models.len() > 1,
            }
            .build(root, children);
        }
    }

    pub(super) fn register_table(&mut self, name: impl AsRef<str>) -> TableId {
        assert!(!self.table_lookup.contains_key(name.as_ref()));
        let id = TableId(self.table_lookup.len());
        self.table_lookup.insert(name.as_ref().to_string(), id);
        id
    }

    fn table_name_from_model(&self, model_name: &Name) -> String {
        let base = std_util::str::pluralize(&model_name.snake_case());
        self.prefix_table_name(&base)
    }

    fn prefix_table_name(&self, name: &str) -> String {
        if let Some(prefix) = &self.builder.table_name_prefix {
            format!("{prefix}{name}")
        } else {
            name.to_string()
        }
    }
}

impl BuildTableFromModels<'_> {
    fn build(&mut self, model: &Model, item_collection_children: Vec<&Model>) {
        // Populate the rest of the columns
        self.map_model_fields(model, !item_collection_children.is_empty());

        let model_column = self.map_model_column(model, &item_collection_children);

        for child in &item_collection_children {
            self.map_item_collection_child(child, model);
        }

        self.update_index_names();

        if let Some(column_id) = model_column {
            self.add_model_column_to_mapping(model, &column_id);
            for child in item_collection_children {
                self.add_model_column_to_mapping(child, &column_id);
            }
        }
    }

    fn map_model_fields(&mut self, model: &Model, has_children: bool) {
        let prefix = if self.prefix_table_names {
            Some(model.name.snake_case())
        } else {
            None
        };

        // First, populate columns
        for field in &model.fields {
            match &field.ty {
                app::FieldTy::Primitive(simple) => {
                    self.create_column_for_primitive(
                        field.id,
                        simple,
                        &field.name,
                        prefix.as_deref(),
                        if has_children && !field.primary_key {
                            // non-PK fields of the root model are always nullable since they don't apply to child rows
                            true
                        } else {
                            field.nullable
                        },
                    );
                }
                // HasMany/HasOne relationships do not have columns... for now?
                app::FieldTy::BelongsTo(_) | app::FieldTy::HasMany(_) | app::FieldTy::HasOne(_) => {
                }
            }
        }

        BuildMapping {
            table: self.table,
            mapping: self.mapping.model_mut(model),
            lowering_columns: vec![],
            model_to_table: vec![],
            model_pk_to_table: vec![],
            table_to_model: vec![],
        }
        .build_mapping(model);

        self.populate_model_indices(model);
    }

    fn populate_model_indices(&mut self, model: &Model) {
        for model_index in &model.indices {
            let mut index = db::Index {
                id: IndexId {
                    table: self.table.id,
                    index: self.table.indices.len(),
                },
                name: String::new(),
                on: self.table.id,
                columns: vec![],
                unique: model_index.unique,
                primary_key: model_index.primary_key,
            };

            self.populate_model_index(model, model_index, &mut index);

            self.table.indices.push(index);
        }
    }

    fn populate_model_index(&mut self, model: &Model, model_index: &app::Index, index: &mut db::Index) {
        for index_field in &model_index.fields {
            let column = self.mapping.model(model.id).fields[index_field.field.index]
            .as_ref()
            .unwrap()
            .column;
            
            match &model.fields[index_field.field.index].ty {
                app::FieldTy::Primitive(_) => index.columns.push(db::IndexColumn {
                    column,
                    op: index_field.op,
                    scope: index_field.scope,
                }),
                app::FieldTy::BelongsTo(_) => todo!(),
                app::FieldTy::HasMany(_) => todo!(),
                app::FieldTy::HasOne(_) => todo!(),
            }
            
            if model_index.primary_key {
                self.table.primary_key.columns.push(column);
            }
        }
    }

    fn create_column_for_primitive(
        &mut self,
        field_id: FieldId,
        primitive: &app::FieldPrimitive,
        name: &app::FieldName,
        prefix: Option<&str>,
        nullable: bool,
    ) {
        let storage_name = if let Some(prefix) = prefix {
            let storage_name = name.storage_name();
            format!("{prefix}__{storage_name}")
        } else {
            name.storage_name().to_owned()
        };

        let storage_ty = db::Type::from_app(
            &primitive.ty,
            primitive.storage_ty.as_ref(),
            &self.db.storage_types,
        )
        .expect("unsupported storage type");

        let column = db::Column {
            id: ColumnId {
                table: self.table.id,
                index: self.table.columns.len(),
            },
            name: storage_name,
            ty: storage_ty.bridge_type(&primitive.ty),
            storage_ty,
            nullable,
            primary_key: false,
        };

        self.mapping.model_mut(field_id.model).fields[field_id.index]
            .as_mut()
            .unwrap()
            .column = column.id;

        self.table.columns.push(column);
    }

    fn update_index_names(&mut self) {
        for index in &mut self.table.indices {
            index.name = format!("index_{}_by", self.table.name);

            for (i, index_column) in index.columns.iter().enumerate() {
                let column = &self.table.columns[index_column.column.index];

                if i > 0 {
                    index.name.push_str("_and");
                }

                index.name.push('_');
                index.name.push_str(&column.name);
            }
        }
    }

    fn map_model_column(&mut self, model: &Model, item_collection_children: &Vec<&Model>) -> Option<ColumnId> {
        if item_collection_children.is_empty() {
            return None;
        }

        let storage_name = String::from("__model");

        let storage_ty = db::Type::VarChar(std::cmp::max(
            model.name.camel_case().len(),
            item_collection_children
                .iter()
                .map(|m| m.name.camel_case().len())
                .max()
                .unwrap_or(0),
        ) as u64);

        let column = db::Column {
            id: ColumnId {
                table: self.table.id,
                index: self.table.columns.len(),
            },
            name: storage_name,
            ty: storage_ty.bridge_type(&stmt::Type::String),
            storage_ty,
            nullable: false,
            primary_key: true,
        };
        let column_id = column.id.clone();

        self.table.columns.push(column);
        self.table.primary_key.columns.push(column_id);

        // add the column to the PK index
        let mut pk_indices: Vec<&mut db::Index> = self
            .table
            .indices
            .iter_mut()
            .filter(|i| i.primary_key)
            .collect();
        if pk_indices.len() != 1 {
            todo!("multiple primary key indices for table {}", self.table.name);
        }
        let pk_index: &mut &mut db::Index = pk_indices
            .iter_mut()
            .next()
            .expect("should be only one index");
        pk_index.columns.push(IndexColumn {
            column: column_id,
            op: db::IndexOp::Eq,
            scope: db::IndexScope::Local,
        });

        Some(column_id)
    }

    fn map_item_collection_child(&mut self, model: &Model, root: &Model) {
        let prefix = Some(model.name.snake_case());

        // First, populate columns
        for field in &model.fields {
            if let Some(parent) = self.find_item_collection_parent_column(field, model) {
                // this field maps to a parent model's field, so use the same column for both
                self.mapping.model_mut(model.id).fields[field.id.index]
                    .as_mut()
                    .unwrap()
                    .column = parent;
                continue;
            }
            match &field.ty {
                app::FieldTy::Primitive(simple) => {
                    self.create_column_for_primitive(
                        field.id,
                        simple,
                        &field.name,
                        prefix.as_deref(),
                        true, // child fields are always nullable
                    );
                }
                // HasMany/HasOne relationships do not have columns... for now?
                app::FieldTy::BelongsTo(_) | app::FieldTy::HasMany(_) | app::FieldTy::HasOne(_) => {
                }
            }
        }

        BuildMapping {
            table: self.table,
            mapping: self.mapping.model_mut(model),
            lowering_columns: vec![],
            model_to_table: vec![],
            model_pk_to_table: vec![],
            table_to_model: vec![],
        }
        .build_mapping(model);

        self.populate_child_model_indices(model, root);
    }

    fn find_item_collection_parent_column(
        &self,
        field: &app::Field,
        model: &Model,
    ) -> Option<ColumnId> {
        let item_collection = &self.mapping.model(model.id).item_collection;
        let parent_field = item_collection.field_mapping.get(&field.id);
        parent_field.map(|fid| {
            let parent_mapping = self.mapping.model(fid.model);
            parent_mapping.fields[fid.index]
                .as_ref()
                .unwrap()
                .column
                .clone()
        })
    }

    fn populate_child_model_indices(&mut self, model: &Model, root: &Model) {
        for model_index in &model.indices {
            let mut temp = self.table.indices.iter_mut().find(|i| i.primary_key);
            let index = temp.as_deref_mut().unwrap();
            if model_index.primary_key {
                for index_field in &model_index.fields {
                    if self.mapping.model(model.id).item_collection.field_mapping.contains_key(&index_field.field) {
                        // this should already be in the index since it comes from the parent model
                        continue;
                    }
                    let column = self.mapping.model(model.id).fields[index_field.field.index]
                    .as_ref()
                    .unwrap()
                    .column;
                    
                    match &model.fields[index_field.field.index].ty {
                        app::FieldTy::Primitive(_) => index.columns.push(db::IndexColumn {
                            column,
                            op: index_field.op,
                            // all the parent columns go in the partition key, child columns go in the local key / sort key
                            scope: db::IndexScope::Local,
                        }),
                        app::FieldTy::BelongsTo(_) => todo!(),
                        app::FieldTy::HasMany(_) => todo!(),
                        app::FieldTy::HasOne(_) => todo!(),
                    }
                    
                    if model_index.primary_key {
                        self.table.primary_key.columns.push(column.clone());
                        let root_mapping = self.mapping.model_mut(root);
                        root_mapping.columns.push(column);
                        // root_mapping.model_pk_to_table.push(stmt::Expr::null());
                        root_mapping.model_to_table.push(stmt::Expr::null());
                        // TODO: also add these columns to the mappings for all models in the item collection
                    }
                }
                continue;
            }
            let mut index = db::Index {
                id: IndexId {
                    table: self.table.id,
                    index: self.table.indices.len(),
                },
                name: String::new(),
                on: self.table.id,
                columns: vec![],
                unique: model_index.unique,
                primary_key: model_index.primary_key,
            };

            self.populate_model_index(model, model_index, &mut index);

            self.table.indices.push(index);
        }
    }

    fn add_model_column_to_mapping(&mut self, model: &Model, column: &ColumnId) {
        let mapping = self.mapping.model_mut(model.id);
        mapping.item_collection.model_column = Some(column.clone());
        mapping.columns.push(column.clone());
        mapping.model_to_table.push(stmt::Value::from(model.name.camel_case()).into());
    }

}

impl BuildMapping<'_> {
    fn build_mapping(mut self, model: &Model) {
        self.map_model_fields_to_columns(model);

        assert!(!self.model_to_table.is_empty());
        assert_eq!(self.model_to_table.len(), self.lowering_columns.len());

        // Iterate fields again (including PK fields) and build the table -> model map.
        for field in &model.fields {
            match &field.ty {
                app::FieldTy::Primitive(primitive) => {
                    let expr = self.map_table_column_to_model(field.id, primitive);
                    self.table_to_model.push(expr);
                }
                app::FieldTy::BelongsTo(_) | app::FieldTy::HasMany(_) | app::FieldTy::HasOne(_) => {
                    self.table_to_model.push(stmt::Value::Null.into());
                }
            }
        }

        // Build the PK lowering
        // for pk_field in &self.table.primary_key.columns {
        //     // Find the column's position in the mapping
        //     let index = self
        //         .lowering_columns
        //         .iter()
        //         .position(|column_id| column_id == pk_field)
        //         .unwrap();

        //     assert!(
        //         index < self.model_to_table.len(),
        //         "column={:#?}; index={}; lowering_columns={:#?}; mapping={:#?}",
        //         pk_field,
        //         index,
        //         self.lowering_columns,
        //         self.model_to_table
        //     );

        //     let expr = self.model_to_table[index].map_projections(|projection| {
        //         let [step, ..] = &projection[..] else {
        //             todo!(
        //                 "projection={:#?}; mapping={:#?}",
        //                 projection,
        //                 self.model_to_table
        //             )
        //         };

        //         for (i, field_id) in model.primary_key.fields.iter().enumerate() {
        //             if field_id.index == *step {
        //                 let mut p = projection.clone();
        //                 p[0] = i;

        //                 return p;
        //             }
        //         }

        //         todo!(
        //             "boom; projection={:?}; mapping={:#?}; PK={:#?}",
        //             projection,
        //             self.model_to_table,
        //             model.primary_key
        //         );
        //     });

        //     self.model_pk_to_table.push(expr);
        // }

        self.mapping.columns = self.lowering_columns;
        self.mapping.model_to_table = stmt::ExprRecord::from_vec(self.model_to_table);
        self.mapping.table_to_model =
            TableToModel::new(stmt::ExprRecord::from_vec(self.table_to_model));
        self.mapping.model_pk_to_table = if self.model_pk_to_table.len() == 1 {
            let expr = self.model_pk_to_table.into_iter().next().unwrap();
            debug_assert!(expr.is_field() || expr.is_cast(), "expr={expr:#?}");
            expr
        } else {
            stmt::ExprRecord::from_vec(self.model_pk_to_table).into()
        };
    }

    fn map_model_fields_to_columns(&mut self, model: &Model) {
        for field in &model.fields {
            match &field.ty {
                app::FieldTy::Primitive(primitive) => {
                    let mapping = self.mapping.fields[field.id.index].as_ref().unwrap();
                    assert_ne!(mapping.column, ColumnId::placeholder());
                    self.map_primitive(field.id, primitive);
                }
                app::FieldTy::BelongsTo(_) | app::FieldTy::HasMany(_) | app::FieldTy::HasOne(_) => {
                }
            }
        }
    }

    fn map_primitive(&mut self, field: FieldId, primitive: &app::FieldPrimitive) {
        let column = self.mapping.fields[field.index].as_ref().unwrap().column;
        let lowering = self.encode_column(column, &primitive.ty, stmt::Expr::ref_self_field(field));

        self.mapping.fields[field.index].as_mut().unwrap().lowering = self.model_to_table.len();

        self.lowering_columns.push(column);
        self.model_to_table.push(lowering);
    }

    fn encode_column(
        &self,
        column_id: ColumnId,
        ty: &stmt::Type,
        expr: impl Into<stmt::Expr>,
    ) -> stmt::Expr {
        let expr = expr.into();
        let column = self.table.column(column_id);

        assert_ne!(stmt::Type::Null, *ty);

        match &column.ty {
            column_ty if column_ty == ty => expr,
            // If the types do not match, attempt casting as a fallback.
            _ => stmt::Expr::cast(expr, &column.ty),
        }
    }

    /// Maps table columns to model field expressions during query lowering.
    ///
    /// Called during query planning to replace model field references with the
    /// appropriate table column expressions. Handles type conversions between
    /// table storage and model types.
    fn map_table_column_to_model(
        &mut self,
        field_id: FieldId,
        primitive: &app::FieldPrimitive,
    ) -> stmt::Expr {
        let column_id = self.mapping.fields[field_id.index].as_ref().unwrap().column;
        let column = self.table.column(column_id);

        // NOTE: nesting and table are stubs here (though often the actual values).
        // The engine must substitute these with the actual TableRef index in the query's TableSource.
        let expr_column = stmt::Expr::column(stmt::ExprColumn {
            nesting: 0,
            table: 0,
            column: column_id.index,
        });

        match &column.ty {
            c_ty if *c_ty == primitive.ty => expr_column,
            // If the types do not match, attempt casting as a fallback.
            _ => stmt::Expr::cast(expr_column, &primitive.ty),
        }
    }
}
