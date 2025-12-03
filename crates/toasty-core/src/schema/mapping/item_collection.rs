use indexmap::IndexMap;

use crate::schema::{app::{FieldId, ModelId}, db::ColumnId};

#[derive(Debug, Clone)]
pub struct ItemCollection {
    /// Path to this model in the item collection hierarchy
    ///
    /// Used to guide how primary key values are computed at the table level.
    /// When the path is empty, this model is not part of a hierarchy so everything
    /// is as normal. When non-empty, the path must end with the model ID of this
    /// model. Each model in the path contributes PK fields which must be
    /// concatenated in a DB-engine-specific way so all models in the collection
    /// can share a table without any ambiguity as to which row belongs to which
    /// model.
    pub path: Vec<ModelId>,

    /// Mapping of this model's fields to parents in the collection
    ///
    /// Used during db schema building to capture relationship information prior to
    /// forming table columns and indices to encapsulate the app schema.
    pub field_mapping: IndexMap<FieldId, FieldId>,

    /// Column that contains the model name
    /// 
    /// Only populated when the path is non-empty.
    pub model_column: Option<ColumnId>
}
