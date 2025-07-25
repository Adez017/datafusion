// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! DFSchema is an extended schema struct that DataFusion uses to provide support for
//! fields with optional relation names.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::hash::Hash;
use std::sync::Arc;

use crate::error::{DataFusionError, Result, _plan_err, _schema_err};
use crate::{
    field_not_found, unqualified_field_not_found, Column, FunctionalDependencies,
    SchemaError, TableReference,
};

use arrow::compute::can_cast_types;
use arrow::datatypes::{
    DataType, Field, FieldRef, Fields, Schema, SchemaBuilder, SchemaRef,
};

/// A reference-counted reference to a [DFSchema].
pub type DFSchemaRef = Arc<DFSchema>;

/// DFSchema wraps an Arrow schema and adds relation names.
///
/// The schema may hold the fields across multiple tables. Some fields may be
/// qualified and some unqualified. A qualified field is a field that has a
/// relation name associated with it.
///
/// Unqualified fields must be unique not only amongst themselves, but also must
/// have a distinct name from any qualified field names. This allows finding a
/// qualified field by name to be possible, so long as there aren't multiple
/// qualified fields with the same name.
///
/// There is an alias to `Arc<DFSchema>` named [DFSchemaRef].
///
/// # Creating qualified schemas
///
/// Use [DFSchema::try_from_qualified_schema] to create a qualified schema from
/// an Arrow schema.
///
/// ```rust
/// use datafusion_common::{DFSchema, Column};
/// use arrow::datatypes::{DataType, Field, Schema};
///
/// let arrow_schema = Schema::new(vec![
///    Field::new("c1", DataType::Int32, false),
/// ]);
///
/// let df_schema = DFSchema::try_from_qualified_schema("t1", &arrow_schema).unwrap();
/// let column = Column::from_qualified_name("t1.c1");
/// assert!(df_schema.has_column(&column));
///
/// // Can also access qualified fields with unqualified name, if it's unambiguous
/// let column = Column::from_qualified_name("c1");
/// assert!(df_schema.has_column(&column));
/// ```
///
/// # Creating unqualified schemas
///
/// Create an unqualified schema using TryFrom:
///
/// ```rust
/// use datafusion_common::{DFSchema, Column};
/// use arrow::datatypes::{DataType, Field, Schema};
///
/// let arrow_schema = Schema::new(vec![
///    Field::new("c1", DataType::Int32, false),
/// ]);
///
/// let df_schema = DFSchema::try_from(arrow_schema).unwrap();
/// let column = Column::new_unqualified("c1");
/// assert!(df_schema.has_column(&column));
/// ```
///
/// # Converting back to Arrow schema
///
/// Use the `Into` trait to convert `DFSchema` into an Arrow schema:
///
/// ```rust
/// use datafusion_common::DFSchema;
/// use arrow::datatypes::{Schema, Field};
/// use std::collections::HashMap;
///
/// let df_schema = DFSchema::from_unqualified_fields(vec![
///    Field::new("c1", arrow::datatypes::DataType::Int32, false),
/// ].into(),HashMap::new()).unwrap();
/// let schema = Schema::from(df_schema);
/// assert_eq!(schema.fields().len(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DFSchema {
    /// Inner Arrow schema reference.
    inner: SchemaRef,
    /// Optional qualifiers for each column in this schema. In the same order as
    /// the `self.inner.fields()`
    field_qualifiers: Vec<Option<TableReference>>,
    /// Stores functional dependencies in the schema.
    functional_dependencies: FunctionalDependencies,
}

impl DFSchema {
    /// Creates an empty `DFSchema`
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(Schema::new([])),
            field_qualifiers: vec![],
            functional_dependencies: FunctionalDependencies::empty(),
        }
    }

    /// Return a reference to the inner Arrow [`Schema`]
    ///
    /// Note this does not have the qualifier information
    pub fn as_arrow(&self) -> &Schema {
        self.inner.as_ref()
    }

    /// Return a reference to the inner Arrow [`SchemaRef`]
    ///
    /// Note this does not have the qualifier information
    pub fn inner(&self) -> &SchemaRef {
        &self.inner
    }

    /// Create a `DFSchema` from an Arrow schema where all the fields have a given qualifier
    pub fn new_with_metadata(
        qualified_fields: Vec<(Option<TableReference>, Arc<Field>)>,
        metadata: HashMap<String, String>,
    ) -> Result<Self> {
        let (qualifiers, fields): (Vec<Option<TableReference>>, Vec<Arc<Field>>) =
            qualified_fields.into_iter().unzip();

        let schema = Arc::new(Schema::new_with_metadata(fields, metadata));

        let dfschema = Self {
            inner: schema,
            field_qualifiers: qualifiers,
            functional_dependencies: FunctionalDependencies::empty(),
        };
        dfschema.check_names()?;
        Ok(dfschema)
    }

    /// Create a new `DFSchema` from a list of Arrow [Field]s
    pub fn from_unqualified_fields(
        fields: Fields,
        metadata: HashMap<String, String>,
    ) -> Result<Self> {
        let field_count = fields.len();
        let schema = Arc::new(Schema::new_with_metadata(fields, metadata));
        let dfschema = Self {
            inner: schema,
            field_qualifiers: vec![None; field_count],
            functional_dependencies: FunctionalDependencies::empty(),
        };
        dfschema.check_names()?;
        Ok(dfschema)
    }

    /// Create a `DFSchema` from an Arrow schema and a given qualifier
    ///
    /// To create a schema from an Arrow schema without a qualifier, use
    /// `DFSchema::try_from`.
    pub fn try_from_qualified_schema(
        qualifier: impl Into<TableReference>,
        schema: &Schema,
    ) -> Result<Self> {
        let qualifier = qualifier.into();
        let schema = DFSchema {
            inner: schema.clone().into(),
            field_qualifiers: vec![Some(qualifier); schema.fields.len()],
            functional_dependencies: FunctionalDependencies::empty(),
        };
        schema.check_names()?;
        Ok(schema)
    }

    /// Create a `DFSchema` from an Arrow schema where all the fields have a given qualifier
    pub fn from_field_specific_qualified_schema(
        qualifiers: Vec<Option<TableReference>>,
        schema: &SchemaRef,
    ) -> Result<Self> {
        let dfschema = Self {
            inner: Arc::clone(schema),
            field_qualifiers: qualifiers,
            functional_dependencies: FunctionalDependencies::empty(),
        };
        dfschema.check_names()?;
        Ok(dfschema)
    }

    /// Return the same schema, where all fields have a given qualifier.
    pub fn with_field_specific_qualified_schema(
        &self,
        qualifiers: Vec<Option<TableReference>>,
    ) -> Result<Self> {
        if qualifiers.len() != self.fields().len() {
            return _plan_err!(
                "Number of qualifiers must match number of fields. Expected {}, got {}",
                self.fields().len(),
                qualifiers.len()
            );
        }
        Ok(DFSchema {
            inner: Arc::clone(&self.inner),
            field_qualifiers: qualifiers,
            functional_dependencies: self.functional_dependencies.clone(),
        })
    }

    /// Check if the schema have some fields with the same name
    pub fn check_names(&self) -> Result<()> {
        let mut qualified_names = BTreeSet::new();
        let mut unqualified_names = BTreeSet::new();

        for (field, qualifier) in self.inner.fields().iter().zip(&self.field_qualifiers) {
            if let Some(qualifier) = qualifier {
                if !qualified_names.insert((qualifier, field.name())) {
                    return _schema_err!(SchemaError::DuplicateQualifiedField {
                        qualifier: Box::new(qualifier.clone()),
                        name: field.name().to_string(),
                    });
                }
            } else if !unqualified_names.insert(field.name()) {
                return _schema_err!(SchemaError::DuplicateUnqualifiedField {
                    name: field.name().to_string()
                });
            }
        }

        for (qualifier, name) in qualified_names {
            if unqualified_names.contains(name) {
                return _schema_err!(SchemaError::AmbiguousReference {
                    field: Box::new(Column::new(Some(qualifier.clone()), name))
                });
            }
        }
        Ok(())
    }

    /// Assigns functional dependencies.
    pub fn with_functional_dependencies(
        mut self,
        functional_dependencies: FunctionalDependencies,
    ) -> Result<Self> {
        if functional_dependencies.is_valid(self.inner.fields.len()) {
            self.functional_dependencies = functional_dependencies;
            Ok(self)
        } else {
            _plan_err!(
                "Invalid functional dependency: {:?}",
                functional_dependencies
            )
        }
    }

    /// Create a new schema that contains the fields from this schema followed by the fields
    /// from the supplied schema. An error will be returned if there are duplicate field names.
    pub fn join(&self, schema: &DFSchema) -> Result<Self> {
        let mut schema_builder = SchemaBuilder::new();
        schema_builder.extend(self.inner.fields().iter().cloned());
        schema_builder.extend(schema.fields().iter().cloned());
        let new_schema = schema_builder.finish();

        let mut new_metadata = self.inner.metadata.clone();
        new_metadata.extend(schema.inner.metadata.clone());
        let new_schema_with_metadata = new_schema.with_metadata(new_metadata);

        let mut new_qualifiers = self.field_qualifiers.clone();
        new_qualifiers.extend_from_slice(schema.field_qualifiers.as_slice());

        let new_self = Self {
            inner: Arc::new(new_schema_with_metadata),
            field_qualifiers: new_qualifiers,
            functional_dependencies: FunctionalDependencies::empty(),
        };
        new_self.check_names()?;
        Ok(new_self)
    }

    /// Modify this schema by appending the fields from the supplied schema, ignoring any
    /// duplicate fields.
    pub fn merge(&mut self, other_schema: &DFSchema) {
        if other_schema.inner.fields.is_empty() {
            return;
        }

        let self_fields: HashSet<(Option<&TableReference>, &FieldRef)> =
            self.iter().collect();
        let self_unqualified_names: HashSet<&str> = self
            .inner
            .fields
            .iter()
            .map(|field| field.name().as_str())
            .collect();

        let mut schema_builder = SchemaBuilder::from(self.inner.fields.clone());
        let mut qualifiers = Vec::new();
        for (qualifier, field) in other_schema.iter() {
            // skip duplicate columns
            let duplicated_field = match qualifier {
                Some(q) => self_fields.contains(&(Some(q), field)),
                // for unqualified columns, check as unqualified name
                None => self_unqualified_names.contains(field.name().as_str()),
            };
            if !duplicated_field {
                schema_builder.push(Arc::clone(field));
                qualifiers.push(qualifier.cloned());
            }
        }
        let mut metadata = self.inner.metadata.clone();
        metadata.extend(other_schema.inner.metadata.clone());

        let finished = schema_builder.finish();
        let finished_with_metadata = finished.with_metadata(metadata);
        self.inner = finished_with_metadata.into();
        self.field_qualifiers.extend(qualifiers);
    }

    /// Get a list of fields
    pub fn fields(&self) -> &Fields {
        &self.inner.fields
    }

    /// Returns an immutable reference of a specific `Field` instance selected using an
    /// offset within the internal `fields` vector
    pub fn field(&self, i: usize) -> &Field {
        &self.inner.fields[i]
    }

    /// Returns an immutable reference of a specific `Field` instance selected using an
    /// offset within the internal `fields` vector and its qualifier
    pub fn qualified_field(&self, i: usize) -> (Option<&TableReference>, &Field) {
        (self.field_qualifiers[i].as_ref(), self.field(i))
    }

    pub fn index_of_column_by_name(
        &self,
        qualifier: Option<&TableReference>,
        name: &str,
    ) -> Option<usize> {
        let mut matches = self
            .iter()
            .enumerate()
            .filter(|(_, (q, f))| match (qualifier, q) {
                // field to lookup is qualified.
                // current field is qualified and not shared between relations, compare both
                // qualifier and name.
                (Some(q), Some(field_q)) => q.resolved_eq(field_q) && f.name() == name,
                // field to lookup is qualified but current field is unqualified.
                (Some(_), None) => false,
                // field to lookup is unqualified, no need to compare qualifier
                (None, Some(_)) | (None, None) => f.name() == name,
            })
            .map(|(idx, _)| idx);
        matches.next()
    }

    /// Find the index of the column with the given qualifier and name,
    /// returning `None` if not found
    ///
    /// See [Self::index_of_column] for a version that returns an error if the
    /// column is not found
    pub fn maybe_index_of_column(&self, col: &Column) -> Option<usize> {
        self.index_of_column_by_name(col.relation.as_ref(), &col.name)
    }

    /// Find the index of the column with the given qualifier and name,
    /// returning `Err` if not found
    ///
    /// See [Self::maybe_index_of_column] for a version that returns `None` if
    /// the column is not found
    pub fn index_of_column(&self, col: &Column) -> Result<usize> {
        self.maybe_index_of_column(col)
            .ok_or_else(|| field_not_found(col.relation.clone(), &col.name, self))
    }

    /// Check if the column is in the current schema
    pub fn is_column_from_schema(&self, col: &Column) -> bool {
        self.index_of_column_by_name(col.relation.as_ref(), &col.name)
            .is_some()
    }

    /// Find the field with the given name
    pub fn field_with_name(
        &self,
        qualifier: Option<&TableReference>,
        name: &str,
    ) -> Result<&Field> {
        if let Some(qualifier) = qualifier {
            self.field_with_qualified_name(qualifier, name)
        } else {
            self.field_with_unqualified_name(name)
        }
    }

    /// Find the qualified field with the given name
    pub fn qualified_field_with_name(
        &self,
        qualifier: Option<&TableReference>,
        name: &str,
    ) -> Result<(Option<&TableReference>, &Field)> {
        if let Some(qualifier) = qualifier {
            let idx = self
                .index_of_column_by_name(Some(qualifier), name)
                .ok_or_else(|| field_not_found(Some(qualifier.clone()), name, self))?;
            Ok((self.field_qualifiers[idx].as_ref(), self.field(idx)))
        } else {
            self.qualified_field_with_unqualified_name(name)
        }
    }

    /// Find all fields having the given qualifier
    pub fn fields_with_qualified(&self, qualifier: &TableReference) -> Vec<&Field> {
        self.iter()
            .filter(|(q, _)| q.map(|q| q.eq(qualifier)).unwrap_or(false))
            .map(|(_, f)| f.as_ref())
            .collect()
    }

    /// Find all fields indices having the given qualifier
    pub fn fields_indices_with_qualified(
        &self,
        qualifier: &TableReference,
    ) -> Vec<usize> {
        self.iter()
            .enumerate()
            .filter_map(|(idx, (q, _))| q.and_then(|q| q.eq(qualifier).then_some(idx)))
            .collect()
    }

    /// Find all fields that match the given name
    pub fn fields_with_unqualified_name(&self, name: &str) -> Vec<&Field> {
        self.fields()
            .iter()
            .filter(|field| field.name() == name)
            .map(|f| f.as_ref())
            .collect()
    }

    /// Find all fields that match the given name and return them with their qualifier
    pub fn qualified_fields_with_unqualified_name(
        &self,
        name: &str,
    ) -> Vec<(Option<&TableReference>, &Field)> {
        self.iter()
            .filter(|(_, field)| field.name() == name)
            .map(|(qualifier, field)| (qualifier, field.as_ref()))
            .collect()
    }

    /// Find all fields that match the given name and convert to column
    pub fn columns_with_unqualified_name(&self, name: &str) -> Vec<Column> {
        self.iter()
            .filter(|(_, field)| field.name() == name)
            .map(|(qualifier, field)| Column::new(qualifier.cloned(), field.name()))
            .collect()
    }

    /// Return all `Column`s for the schema
    pub fn columns(&self) -> Vec<Column> {
        self.iter()
            .map(|(qualifier, field)| {
                Column::new(qualifier.cloned(), field.name().clone())
            })
            .collect()
    }

    /// Find the qualified field with the given unqualified name
    pub fn qualified_field_with_unqualified_name(
        &self,
        name: &str,
    ) -> Result<(Option<&TableReference>, &Field)> {
        let matches = self.qualified_fields_with_unqualified_name(name);
        match matches.len() {
            0 => Err(unqualified_field_not_found(name, self)),
            1 => Ok((matches[0].0, matches[0].1)),
            _ => {
                // When `matches` size > 1, it doesn't necessarily mean an `ambiguous name` problem.
                // Because name may generate from Alias/... . It means that it don't own qualifier.
                // For example:
                //             Join on id = b.id
                // Project a.id as id   TableScan b id
                // In this case, there isn't `ambiguous name` problem. When `matches` just contains
                // one field without qualifier, we should return it.
                let fields_without_qualifier = matches
                    .iter()
                    .filter(|(q, _)| q.is_none())
                    .collect::<Vec<_>>();
                if fields_without_qualifier.len() == 1 {
                    Ok((fields_without_qualifier[0].0, fields_without_qualifier[0].1))
                } else {
                    _schema_err!(SchemaError::AmbiguousReference {
                        field: Box::new(Column::new_unqualified(name.to_string()))
                    })
                }
            }
        }
    }

    /// Find the field with the given name
    pub fn field_with_unqualified_name(&self, name: &str) -> Result<&Field> {
        self.qualified_field_with_unqualified_name(name)
            .map(|(_, field)| field)
    }

    /// Find the field with the given qualified name
    pub fn field_with_qualified_name(
        &self,
        qualifier: &TableReference,
        name: &str,
    ) -> Result<&Field> {
        let idx = self
            .index_of_column_by_name(Some(qualifier), name)
            .ok_or_else(|| field_not_found(Some(qualifier.clone()), name, self))?;

        Ok(self.field(idx))
    }

    /// Find the field with the given qualified column
    pub fn qualified_field_from_column(
        &self,
        column: &Column,
    ) -> Result<(Option<&TableReference>, &Field)> {
        self.qualified_field_with_name(column.relation.as_ref(), &column.name)
    }

    /// Find if the field exists with the given name
    pub fn has_column_with_unqualified_name(&self, name: &str) -> bool {
        self.fields().iter().any(|field| field.name() == name)
    }

    /// Find if the field exists with the given qualified name
    pub fn has_column_with_qualified_name(
        &self,
        qualifier: &TableReference,
        name: &str,
    ) -> bool {
        self.iter()
            .any(|(q, f)| q.map(|q| q.eq(qualifier)).unwrap_or(false) && f.name() == name)
    }

    /// Find if the field exists with the given qualified column
    pub fn has_column(&self, column: &Column) -> bool {
        match &column.relation {
            Some(r) => self.has_column_with_qualified_name(r, &column.name),
            None => self.has_column_with_unqualified_name(&column.name),
        }
    }

    /// Check to see if unqualified field names matches field names in Arrow schema
    pub fn matches_arrow_schema(&self, arrow_schema: &Schema) -> bool {
        self.inner
            .fields
            .iter()
            .zip(arrow_schema.fields().iter())
            .all(|(dffield, arrowfield)| dffield.name() == arrowfield.name())
    }

    /// Check to see if fields in 2 Arrow schemas are compatible
    #[deprecated(since = "47.0.0", note = "This method is no longer used")]
    pub fn check_arrow_schema_type_compatible(
        &self,
        arrow_schema: &Schema,
    ) -> Result<()> {
        let self_arrow_schema: Schema = self.into();
        self_arrow_schema
            .fields()
            .iter()
            .zip(arrow_schema.fields().iter())
            .try_for_each(|(l_field, r_field)| {
                if !can_cast_types(r_field.data_type(), l_field.data_type()) {
                    _plan_err!("Column {} (type: {}) is not compatible with column {} (type: {})",
                                r_field.name(),
                                r_field.data_type(),
                                l_field.name(),
                                l_field.data_type())
                } else {
                    Ok(())
                }
            })
    }

    /// Returns true if the two schemas have the same qualified named
    /// fields with logically equivalent data types. Returns false otherwise.
    ///
    /// Use [DFSchema]::equivalent_names_and_types for stricter semantic type
    /// equivalence checking.
    pub fn logically_equivalent_names_and_types(&self, other: &Self) -> bool {
        if self.fields().len() != other.fields().len() {
            return false;
        }
        let self_fields = self.iter();
        let other_fields = other.iter();
        self_fields.zip(other_fields).all(|((q1, f1), (q2, f2))| {
            q1 == q2
                && f1.name() == f2.name()
                && Self::datatype_is_logically_equal(f1.data_type(), f2.data_type())
        })
    }

    #[deprecated(since = "47.0.0", note = "Use has_equivalent_names_and_types` instead")]
    pub fn equivalent_names_and_types(&self, other: &Self) -> bool {
        self.has_equivalent_names_and_types(other).is_ok()
    }

    /// Returns Ok if the two schemas have the same qualified named
    /// fields with the compatible data types.
    ///
    /// Returns an `Err` with a message otherwise.
    ///
    /// This is a specialized version of Eq that ignores differences in
    /// nullability and metadata.
    ///
    /// Use [DFSchema]::logically_equivalent_names_and_types for a weaker
    /// logical type checking, which for example would consider a dictionary
    /// encoded UTF8 array to be equivalent to a plain UTF8 array.
    pub fn has_equivalent_names_and_types(&self, other: &Self) -> Result<()> {
        // case 1 : schema length mismatch
        if self.fields().len() != other.fields().len() {
            _plan_err!(
                "Schema mismatch: the schema length are not same \
            Expected schema length: {}, got: {}",
                self.fields().len(),
                other.fields().len()
            )
        } else {
            // case 2 : schema length match, but fields mismatch
            // check if the fields name are the same and have the same data types
            self.fields()
                .iter()
                .zip(other.fields().iter())
                .try_for_each(|(f1, f2)| {
                    if f1.name() != f2.name()
                        || (!DFSchema::datatype_is_semantically_equal(
                            f1.data_type(),
                            f2.data_type(),
                        ))
                    {
                        _plan_err!(
                            "Schema mismatch: Expected field '{}' with type {:?}, \
                            but got '{}' with type {:?}.",
                            f1.name(),
                            f1.data_type(),
                            f2.name(),
                            f2.data_type()
                        )
                    } else {
                        Ok(())
                    }
                })
        }
    }

    /// Checks if two [`DataType`]s are logically equal. This is a notably weaker constraint
    /// than datatype_is_semantically_equal in that different representations of same data can be
    /// logically but not semantically equivalent. Semantically equivalent types are always also
    /// logically equivalent. For example:
    /// - a Dictionary<K,V> type is logically equal to a plain V type
    /// - a Dictionary<K1, V1> is also logically equal to Dictionary<K2, V1>
    /// - Utf8 and Utf8View are logically equal
    pub fn datatype_is_logically_equal(dt1: &DataType, dt2: &DataType) -> bool {
        // check nested fields
        match (dt1, dt2) {
            (DataType::Dictionary(_, v1), DataType::Dictionary(_, v2)) => {
                v1.as_ref() == v2.as_ref()
            }
            (DataType::Dictionary(_, v1), othertype) => v1.as_ref() == othertype,
            (othertype, DataType::Dictionary(_, v1)) => v1.as_ref() == othertype,
            (DataType::List(f1), DataType::List(f2))
            | (DataType::LargeList(f1), DataType::LargeList(f2))
            | (DataType::FixedSizeList(f1, _), DataType::FixedSizeList(f2, _)) => {
                // Don't compare the names of the technical inner field
                // Usually "item" but that's not mandated
                Self::datatype_is_logically_equal(f1.data_type(), f2.data_type())
            }
            (DataType::Map(f1, _), DataType::Map(f2, _)) => {
                // Don't compare the names of the technical inner fields
                // Usually "entries", "key", "value" but that's not mandated
                match (f1.data_type(), f2.data_type()) {
                    (DataType::Struct(f1_inner), DataType::Struct(f2_inner)) => {
                        f1_inner.len() == f2_inner.len()
                            && f1_inner.iter().zip(f2_inner.iter()).all(|(f1, f2)| {
                                Self::datatype_is_logically_equal(
                                    f1.data_type(),
                                    f2.data_type(),
                                )
                            })
                    }
                    _ => panic!("Map type should have an inner struct field"),
                }
            }
            (DataType::Struct(fields1), DataType::Struct(fields2)) => {
                let iter1 = fields1.iter();
                let iter2 = fields2.iter();
                fields1.len() == fields2.len() &&
                        // all fields have to be the same
                    iter1
                    .zip(iter2)
                        .all(|(f1, f2)| Self::field_is_logically_equal(f1, f2))
            }
            (DataType::Union(fields1, _), DataType::Union(fields2, _)) => {
                let iter1 = fields1.iter();
                let iter2 = fields2.iter();
                fields1.len() == fields2.len() &&
                    // all fields have to be the same
                    iter1
                        .zip(iter2)
                        .all(|((t1, f1), (t2, f2))| t1 == t2 && Self::field_is_logically_equal(f1, f2))
            }
            // Utf8 and Utf8View are logically equivalent
            (DataType::Utf8, DataType::Utf8View) => true,
            (DataType::Utf8View, DataType::Utf8) => true,
            _ => Self::datatype_is_semantically_equal(dt1, dt2),
        }
    }

    /// Returns true of two [`DataType`]s are semantically equal (same
    /// name and type), ignoring both metadata and nullability, and decimal precision/scale.
    ///
    /// request to upstream: <https://github.com/apache/arrow-rs/issues/3199>
    pub fn datatype_is_semantically_equal(dt1: &DataType, dt2: &DataType) -> bool {
        // check nested fields
        match (dt1, dt2) {
            (DataType::Dictionary(k1, v1), DataType::Dictionary(k2, v2)) => {
                Self::datatype_is_semantically_equal(k1.as_ref(), k2.as_ref())
                    && Self::datatype_is_semantically_equal(v1.as_ref(), v2.as_ref())
            }
            (DataType::List(f1), DataType::List(f2))
            | (DataType::LargeList(f1), DataType::LargeList(f2))
            | (DataType::FixedSizeList(f1, _), DataType::FixedSizeList(f2, _)) => {
                // Don't compare the names of the technical inner field
                // Usually "item" but that's not mandated
                Self::datatype_is_semantically_equal(f1.data_type(), f2.data_type())
            }
            (DataType::Map(f1, _), DataType::Map(f2, _)) => {
                // Don't compare the names of the technical inner fields
                // Usually "entries", "key", "value" but that's not mandated
                match (f1.data_type(), f2.data_type()) {
                    (DataType::Struct(f1_inner), DataType::Struct(f2_inner)) => {
                        f1_inner.len() == f2_inner.len()
                            && f1_inner.iter().zip(f2_inner.iter()).all(|(f1, f2)| {
                                Self::datatype_is_semantically_equal(
                                    f1.data_type(),
                                    f2.data_type(),
                                )
                            })
                    }
                    _ => panic!("Map type should have an inner struct field"),
                }
            }
            (DataType::Struct(fields1), DataType::Struct(fields2)) => {
                let iter1 = fields1.iter();
                let iter2 = fields2.iter();
                fields1.len() == fields2.len() &&
                        // all fields have to be the same
                    iter1
                    .zip(iter2)
                        .all(|(f1, f2)| Self::field_is_semantically_equal(f1, f2))
            }
            (DataType::Union(fields1, _), DataType::Union(fields2, _)) => {
                let iter1 = fields1.iter();
                let iter2 = fields2.iter();
                fields1.len() == fields2.len() &&
                    // all fields have to be the same
                    iter1
                        .zip(iter2)
                        .all(|((t1, f1), (t2, f2))| t1 == t2 && Self::field_is_semantically_equal(f1, f2))
            }
            (
                DataType::Decimal128(_l_precision, _l_scale),
                DataType::Decimal128(_r_precision, _r_scale),
            ) => true,
            (
                DataType::Decimal256(_l_precision, _l_scale),
                DataType::Decimal256(_r_precision, _r_scale),
            ) => true,
            _ => dt1 == dt2,
        }
    }

    fn field_is_logically_equal(f1: &Field, f2: &Field) -> bool {
        f1.name() == f2.name()
            && Self::datatype_is_logically_equal(f1.data_type(), f2.data_type())
    }

    fn field_is_semantically_equal(f1: &Field, f2: &Field) -> bool {
        f1.name() == f2.name()
            && Self::datatype_is_semantically_equal(f1.data_type(), f2.data_type())
    }

    /// Strip all field qualifier in schema
    pub fn strip_qualifiers(self) -> Self {
        DFSchema {
            field_qualifiers: vec![None; self.inner.fields.len()],
            inner: self.inner,
            functional_dependencies: self.functional_dependencies,
        }
    }

    /// Replace all field qualifier with new value in schema
    pub fn replace_qualifier(self, qualifier: impl Into<TableReference>) -> Self {
        let qualifier = qualifier.into();
        DFSchema {
            field_qualifiers: vec![Some(qualifier); self.inner.fields.len()],
            inner: self.inner,
            functional_dependencies: self.functional_dependencies,
        }
    }

    /// Get list of fully-qualified field names in this schema
    pub fn field_names(&self) -> Vec<String> {
        self.iter()
            .map(|(qualifier, field)| qualified_name(qualifier, field.name()))
            .collect::<Vec<_>>()
    }

    /// Get metadata of this schema
    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.inner.metadata
    }

    /// Get functional dependencies
    pub fn functional_dependencies(&self) -> &FunctionalDependencies {
        &self.functional_dependencies
    }

    /// Iterate over the qualifiers and fields in the DFSchema
    pub fn iter(&self) -> impl Iterator<Item = (Option<&TableReference>, &FieldRef)> {
        self.field_qualifiers
            .iter()
            .zip(self.inner.fields().iter())
            .map(|(qualifier, field)| (qualifier.as_ref(), field))
    }
}

impl From<DFSchema> for Schema {
    /// Convert DFSchema into a Schema
    fn from(df_schema: DFSchema) -> Self {
        let fields: Fields = df_schema.inner.fields.clone();
        Schema::new_with_metadata(fields, df_schema.inner.metadata.clone())
    }
}

impl From<&DFSchema> for Schema {
    /// Convert DFSchema reference into a Schema
    fn from(df_schema: &DFSchema) -> Self {
        let fields: Fields = df_schema.inner.fields.clone();
        Schema::new_with_metadata(fields, df_schema.inner.metadata.clone())
    }
}

/// Allow DFSchema to be converted into an Arrow `&Schema`
impl AsRef<Schema> for DFSchema {
    fn as_ref(&self) -> &Schema {
        self.as_arrow()
    }
}

/// Allow DFSchema to be converted into an Arrow `&SchemaRef` (to clone, for
/// example)
impl AsRef<SchemaRef> for DFSchema {
    fn as_ref(&self) -> &SchemaRef {
        self.inner()
    }
}

/// Create a `DFSchema` from an Arrow schema
impl TryFrom<Schema> for DFSchema {
    type Error = DataFusionError;
    fn try_from(schema: Schema) -> Result<Self, Self::Error> {
        Self::try_from(Arc::new(schema))
    }
}

impl TryFrom<SchemaRef> for DFSchema {
    type Error = DataFusionError;
    fn try_from(schema: SchemaRef) -> Result<Self, Self::Error> {
        let field_count = schema.fields.len();
        let dfschema = Self {
            inner: schema,
            field_qualifiers: vec![None; field_count],
            functional_dependencies: FunctionalDependencies::empty(),
        };
        Ok(dfschema)
    }
}

impl From<DFSchema> for SchemaRef {
    fn from(df_schema: DFSchema) -> Self {
        SchemaRef::new(df_schema.into())
    }
}

// Hashing refers to a subset of fields considered in PartialEq.
impl Hash for DFSchema {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.inner.fields.hash(state);
        self.inner.metadata.len().hash(state); // HashMap is not hashable
    }
}

/// Convenience trait to convert Schema like things to DFSchema and DFSchemaRef with fewer keystrokes
pub trait ToDFSchema
where
    Self: Sized,
{
    /// Attempt to create a DSSchema
    fn to_dfschema(self) -> Result<DFSchema>;

    /// Attempt to create a DSSchemaRef
    fn to_dfschema_ref(self) -> Result<DFSchemaRef> {
        Ok(Arc::new(self.to_dfschema()?))
    }
}

impl ToDFSchema for Schema {
    fn to_dfschema(self) -> Result<DFSchema> {
        DFSchema::try_from(self)
    }
}

impl ToDFSchema for SchemaRef {
    fn to_dfschema(self) -> Result<DFSchema> {
        DFSchema::try_from(self)
    }
}

impl ToDFSchema for Vec<Field> {
    fn to_dfschema(self) -> Result<DFSchema> {
        let field_count = self.len();
        let schema = Schema {
            fields: self.into(),
            metadata: HashMap::new(),
        };
        let dfschema = DFSchema {
            inner: schema.into(),
            field_qualifiers: vec![None; field_count],
            functional_dependencies: FunctionalDependencies::empty(),
        };
        Ok(dfschema)
    }
}

impl Display for DFSchema {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "fields:[{}], metadata:{:?}",
            self.iter()
                .map(|(q, f)| qualified_name(q, f.name()))
                .collect::<Vec<String>>()
                .join(", "),
            self.inner.metadata
        )
    }
}

/// Provides schema information needed by certain methods of `Expr`
/// (defined in the datafusion-common crate).
///
/// Note that this trait is implemented for &[DFSchema] which is
/// widely used in the DataFusion codebase.
pub trait ExprSchema: std::fmt::Debug {
    /// Is this column reference nullable?
    fn nullable(&self, col: &Column) -> Result<bool> {
        Ok(self.field_from_column(col)?.is_nullable())
    }

    /// What is the datatype of this column?
    fn data_type(&self, col: &Column) -> Result<&DataType> {
        Ok(self.field_from_column(col)?.data_type())
    }

    /// Returns the column's optional metadata.
    fn metadata(&self, col: &Column) -> Result<&HashMap<String, String>> {
        Ok(self.field_from_column(col)?.metadata())
    }

    /// Return the column's datatype and nullability
    fn data_type_and_nullable(&self, col: &Column) -> Result<(&DataType, bool)> {
        let field = self.field_from_column(col)?;
        Ok((field.data_type(), field.is_nullable()))
    }

    // Return the column's field
    fn field_from_column(&self, col: &Column) -> Result<&Field>;
}

// Implement `ExprSchema` for `Arc<DFSchema>`
impl<P: AsRef<DFSchema> + std::fmt::Debug> ExprSchema for P {
    fn nullable(&self, col: &Column) -> Result<bool> {
        self.as_ref().nullable(col)
    }

    fn data_type(&self, col: &Column) -> Result<&DataType> {
        self.as_ref().data_type(col)
    }

    fn metadata(&self, col: &Column) -> Result<&HashMap<String, String>> {
        ExprSchema::metadata(self.as_ref(), col)
    }

    fn data_type_and_nullable(&self, col: &Column) -> Result<(&DataType, bool)> {
        self.as_ref().data_type_and_nullable(col)
    }

    fn field_from_column(&self, col: &Column) -> Result<&Field> {
        self.as_ref().field_from_column(col)
    }
}

impl ExprSchema for DFSchema {
    fn field_from_column(&self, col: &Column) -> Result<&Field> {
        match &col.relation {
            Some(r) => self.field_with_qualified_name(r, &col.name),
            None => self.field_with_unqualified_name(&col.name),
        }
    }
}

/// DataFusion-specific extensions to [`Schema`].
pub trait SchemaExt {
    /// This is a specialized version of Eq that ignores differences
    /// in nullability and metadata.
    ///
    /// It works the same as [`DFSchema::equivalent_names_and_types`].
    fn equivalent_names_and_types(&self, other: &Self) -> bool;

    /// Returns nothing if the two schemas have the same qualified named
    /// fields with logically equivalent data types. Returns internal error otherwise.
    ///
    /// Use [DFSchema]::equivalent_names_and_types for stricter semantic type
    /// equivalence checking.
    ///
    /// It is only used by insert into cases.
    fn logically_equivalent_names_and_types(&self, other: &Self) -> Result<()>;
}

impl SchemaExt for Schema {
    fn equivalent_names_and_types(&self, other: &Self) -> bool {
        if self.fields().len() != other.fields().len() {
            return false;
        }

        self.fields()
            .iter()
            .zip(other.fields().iter())
            .all(|(f1, f2)| {
                f1.name() == f2.name()
                    && DFSchema::datatype_is_semantically_equal(
                        f1.data_type(),
                        f2.data_type(),
                    )
            })
    }

    // It is only used by insert into cases.
    fn logically_equivalent_names_and_types(&self, other: &Self) -> Result<()> {
        // case 1 : schema length mismatch
        if self.fields().len() != other.fields().len() {
            _plan_err!(
                "Inserting query must have the same schema length as the table. \
            Expected table schema length: {}, got: {}",
                self.fields().len(),
                other.fields().len()
            )
        } else {
            // case 2 : schema length match, but fields mismatch
            // check if the fields name are the same and have the same data types
            self.fields()
                .iter()
                .zip(other.fields().iter())
                .try_for_each(|(f1, f2)| {
                    if f1.name() != f2.name() || (!DFSchema::datatype_is_logically_equal(f1.data_type(), f2.data_type()) && !can_cast_types(f2.data_type(), f1.data_type())) {
                        _plan_err!(
                            "Inserting query schema mismatch: Expected table field '{}' with type {:?}, \
                            but got '{}' with type {:?}.",
                            f1.name(),
                            f1.data_type(),
                            f2.name(),
                            f2.data_type())
                    } else {
                        Ok(())
                    }
                })
        }
    }
}

pub fn qualified_name(qualifier: Option<&TableReference>, name: &str) -> String {
    match qualifier {
        Some(q) => format!("{q}.{name}"),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crate::assert_contains;

    use super::*;

    #[test]
    fn qualifier_in_name() -> Result<()> {
        let col = Column::from_name("t1.c0");
        let schema = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        // lookup with unqualified name "t1.c0"
        let err = schema.index_of_column(&col).unwrap_err();
        let expected = "Schema error: No field named \"t1.c0\". \
            Column names are case sensitive. \
            You can use double quotes to refer to the \"\"t1.c0\"\" column \
            or set the datafusion.sql_parser.enable_ident_normalization configuration. \
            Did you mean 't1.c0'?.";
        assert_eq!(err.strip_backtrace(), expected);
        Ok(())
    }

    #[test]
    fn quoted_qualifiers_in_name() -> Result<()> {
        let col = Column::from_name("t1.c0");
        let schema = DFSchema::try_from_qualified_schema(
            "t1",
            &Schema::new(vec![
                Field::new("CapitalColumn", DataType::Boolean, true),
                Field::new("field.with.period", DataType::Boolean, true),
            ]),
        )?;

        // lookup with unqualified name "t1.c0"
        let err = schema.index_of_column(&col).unwrap_err();
        let expected = "Schema error: No field named \"t1.c0\". \
            Valid fields are t1.\"CapitalColumn\", t1.\"field.with.period\".";
        assert_eq!(err.strip_backtrace(), expected);
        Ok(())
    }

    #[test]
    fn from_unqualified_schema() -> Result<()> {
        let schema = DFSchema::try_from(test_schema_1())?;
        assert_eq!("fields:[c0, c1], metadata:{}", schema.to_string());
        Ok(())
    }

    #[test]
    fn from_qualified_schema() -> Result<()> {
        let schema = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        assert_eq!("fields:[t1.c0, t1.c1], metadata:{}", schema.to_string());
        Ok(())
    }

    #[test]
    fn test_from_field_specific_qualified_schema() -> Result<()> {
        let schema = DFSchema::from_field_specific_qualified_schema(
            vec![Some("t1".into()), None],
            &Arc::new(Schema::new(vec![
                Field::new("c0", DataType::Boolean, true),
                Field::new("c1", DataType::Boolean, true),
            ])),
        )?;
        assert_eq!("fields:[t1.c0, c1], metadata:{}", schema.to_string());
        Ok(())
    }

    #[test]
    fn test_from_qualified_fields() -> Result<()> {
        let schema = DFSchema::new_with_metadata(
            vec![
                (
                    Some("t0".into()),
                    Arc::new(Field::new("c0", DataType::Boolean, true)),
                ),
                (None, Arc::new(Field::new("c1", DataType::Boolean, true))),
            ],
            HashMap::new(),
        )?;
        assert_eq!("fields:[t0.c0, c1], metadata:{}", schema.to_string());
        Ok(())
    }

    #[test]
    fn from_qualified_schema_into_arrow_schema() -> Result<()> {
        let schema = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        let arrow_schema: Schema = schema.into();
        let expected = "Field { name: \"c0\", data_type: Boolean, nullable: true, dict_id: 0, dict_is_ordered: false, metadata: {} }, \
        Field { name: \"c1\", data_type: Boolean, nullable: true, dict_id: 0, dict_is_ordered: false, metadata: {} }";
        assert_eq!(expected, arrow_schema.to_string());
        Ok(())
    }

    #[test]
    fn join_qualified() -> Result<()> {
        let left = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        let right = DFSchema::try_from_qualified_schema("t2", &test_schema_1())?;
        let join = left.join(&right)?;
        assert_eq!(
            "fields:[t1.c0, t1.c1, t2.c0, t2.c1], metadata:{}",
            join.to_string()
        );
        // test valid access
        assert!(join
            .field_with_qualified_name(&TableReference::bare("t1"), "c0")
            .is_ok());
        assert!(join
            .field_with_qualified_name(&TableReference::bare("t2"), "c0")
            .is_ok());
        // test invalid access
        assert!(join.field_with_unqualified_name("c0").is_err());
        assert!(join.field_with_unqualified_name("t1.c0").is_err());
        assert!(join.field_with_unqualified_name("t2.c0").is_err());
        Ok(())
    }

    #[test]
    fn join_qualified_duplicate() -> Result<()> {
        let left = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        let right = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        let join = left.join(&right);
        assert_eq!(
            join.unwrap_err().strip_backtrace(),
            "Schema error: Schema contains duplicate qualified field name t1.c0",
        );
        Ok(())
    }

    #[test]
    fn join_unqualified_duplicate() -> Result<()> {
        let left = DFSchema::try_from(test_schema_1())?;
        let right = DFSchema::try_from(test_schema_1())?;
        let join = left.join(&right);
        assert_eq!(
            join.unwrap_err().strip_backtrace(),
            "Schema error: Schema contains duplicate unqualified field name c0"
        );
        Ok(())
    }

    #[test]
    fn join_mixed() -> Result<()> {
        let left = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        let right = DFSchema::try_from(test_schema_2())?;
        let join = left.join(&right)?;
        assert_eq!(
            "fields:[t1.c0, t1.c1, c100, c101], metadata:{}",
            join.to_string()
        );
        // test valid access
        assert!(join
            .field_with_qualified_name(&TableReference::bare("t1"), "c0")
            .is_ok());
        assert!(join.field_with_unqualified_name("c0").is_ok());
        assert!(join.field_with_unqualified_name("c100").is_ok());
        assert!(join.field_with_name(None, "c100").is_ok());
        // test invalid access
        assert!(join.field_with_unqualified_name("t1.c0").is_err());
        assert!(join.field_with_unqualified_name("t1.c100").is_err());
        assert!(join
            .field_with_qualified_name(&TableReference::bare(""), "c100")
            .is_err());
        Ok(())
    }

    #[test]
    fn join_mixed_duplicate() -> Result<()> {
        let left = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        let right = DFSchema::try_from(test_schema_1())?;
        let join = left.join(&right);
        assert_contains!(join.unwrap_err().to_string(),
                         "Schema error: Schema contains qualified \
                          field name t1.c0 and unqualified field name c0 which would be ambiguous");
        Ok(())
    }

    #[test]
    fn helpful_error_messages() -> Result<()> {
        let schema = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
        let expected_help = "Valid fields are t1.c0, t1.c1.";
        assert_contains!(
            schema
                .field_with_qualified_name(&TableReference::bare("x"), "y")
                .unwrap_err()
                .to_string(),
            expected_help
        );
        assert_contains!(
            schema
                .field_with_unqualified_name("y")
                .unwrap_err()
                .to_string(),
            expected_help
        );
        assert!(schema.index_of_column_by_name(None, "y").is_none());
        assert!(schema.index_of_column_by_name(None, "t1.c0").is_none());

        Ok(())
    }

    #[test]
    fn select_without_valid_fields() {
        let schema = DFSchema::empty();

        let col = Column::from_qualified_name("t1.c0");
        let err = schema.index_of_column(&col).unwrap_err();
        let expected = "Schema error: No field named t1.c0.";
        assert_eq!(err.strip_backtrace(), expected);

        // the same check without qualifier
        let col = Column::from_name("c0");
        let err = schema.index_of_column(&col).err().unwrap();
        let expected = "Schema error: No field named c0.";
        assert_eq!(err.strip_backtrace(), expected);
    }

    #[test]
    fn into() {
        // Demonstrate how to convert back and forth between Schema, SchemaRef, DFSchema, and DFSchemaRef
        let arrow_schema = Schema::new_with_metadata(
            vec![Field::new("c0", DataType::Int64, true)],
            test_metadata(),
        );
        let arrow_schema_ref = Arc::new(arrow_schema.clone());

        let df_schema = DFSchema {
            inner: Arc::clone(&arrow_schema_ref),
            field_qualifiers: vec![None; arrow_schema_ref.fields.len()],
            functional_dependencies: FunctionalDependencies::empty(),
        };
        let df_schema_ref = Arc::new(df_schema.clone());

        {
            let arrow_schema = arrow_schema.clone();
            let arrow_schema_ref = Arc::clone(&arrow_schema_ref);

            assert_eq!(df_schema, arrow_schema.to_dfschema().unwrap());
            assert_eq!(df_schema, arrow_schema_ref.to_dfschema().unwrap());
        }

        {
            let arrow_schema = arrow_schema.clone();
            let arrow_schema_ref = Arc::clone(&arrow_schema_ref);

            assert_eq!(df_schema_ref, arrow_schema.to_dfschema_ref().unwrap());
            assert_eq!(df_schema_ref, arrow_schema_ref.to_dfschema_ref().unwrap());
        }

        // Now, consume the refs
        assert_eq!(df_schema_ref, arrow_schema.to_dfschema_ref().unwrap());
        assert_eq!(df_schema_ref, arrow_schema_ref.to_dfschema_ref().unwrap());
    }

    fn test_schema_1() -> Schema {
        Schema::new(vec![
            Field::new("c0", DataType::Boolean, true),
            Field::new("c1", DataType::Boolean, true),
        ])
    }
    #[test]
    fn test_dfschema_to_schema_conversion() {
        let mut a_metadata = HashMap::new();
        a_metadata.insert("key".to_string(), "value".to_string());
        let a_field = Field::new("a", DataType::Int64, false).with_metadata(a_metadata);

        let mut b_metadata = HashMap::new();
        b_metadata.insert("key".to_string(), "value".to_string());
        let b_field = Field::new("b", DataType::Int64, false).with_metadata(b_metadata);

        let schema = Arc::new(Schema::new(vec![a_field, b_field]));

        let df_schema = DFSchema {
            inner: Arc::clone(&schema),
            field_qualifiers: vec![None; schema.fields.len()],
            functional_dependencies: FunctionalDependencies::empty(),
        };

        assert_eq!(df_schema.inner.metadata(), schema.metadata())
    }

    #[test]
    fn test_contain_column() -> Result<()> {
        // qualified exists
        {
            let col = Column::from_qualified_name("t1.c0");
            let schema = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
            assert!(schema.is_column_from_schema(&col));
        }

        // qualified not exists
        {
            let col = Column::from_qualified_name("t1.c2");
            let schema = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
            assert!(!schema.is_column_from_schema(&col));
        }

        // unqualified exists
        {
            let col = Column::from_name("c0");
            let schema = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
            assert!(schema.is_column_from_schema(&col));
        }

        // unqualified not exists
        {
            let col = Column::from_name("c2");
            let schema = DFSchema::try_from_qualified_schema("t1", &test_schema_1())?;
            assert!(!schema.is_column_from_schema(&col));
        }

        Ok(())
    }

    #[test]
    fn test_datatype_is_logically_equal() {
        assert!(DFSchema::datatype_is_logically_equal(
            &DataType::Int8,
            &DataType::Int8
        ));

        assert!(!DFSchema::datatype_is_logically_equal(
            &DataType::Int8,
            &DataType::Int16
        ));

        // Test lists

        // Succeeds if both have the same element type, disregards names and nullability
        assert!(DFSchema::datatype_is_logically_equal(
            &DataType::List(Field::new_list_field(DataType::Int8, true).into()),
            &DataType::List(Field::new("element", DataType::Int8, false).into())
        ));

        // Fails if element type is different
        assert!(!DFSchema::datatype_is_logically_equal(
            &DataType::List(Field::new_list_field(DataType::Int8, true).into()),
            &DataType::List(Field::new_list_field(DataType::Int16, true).into())
        ));

        // Test maps
        let map_field = DataType::Map(
            Field::new(
                "entries",
                DataType::Struct(Fields::from(vec![
                    Field::new("key", DataType::Int8, false),
                    Field::new("value", DataType::Int8, true),
                ])),
                true,
            )
            .into(),
            true,
        );

        // Succeeds if both maps have the same key and value types, disregards names and nullability
        assert!(DFSchema::datatype_is_logically_equal(
            &map_field,
            &DataType::Map(
                Field::new(
                    "pairs",
                    DataType::Struct(Fields::from(vec![
                        Field::new("one", DataType::Int8, false),
                        Field::new("two", DataType::Int8, false)
                    ])),
                    true
                )
                .into(),
                true
            )
        ));
        // Fails if value type is different
        assert!(!DFSchema::datatype_is_logically_equal(
            &map_field,
            &DataType::Map(
                Field::new(
                    "entries",
                    DataType::Struct(Fields::from(vec![
                        Field::new("key", DataType::Int8, false),
                        Field::new("value", DataType::Int16, true)
                    ])),
                    true
                )
                .into(),
                true
            )
        ));

        // Fails if key type is different
        assert!(!DFSchema::datatype_is_logically_equal(
            &map_field,
            &DataType::Map(
                Field::new(
                    "entries",
                    DataType::Struct(Fields::from(vec![
                        Field::new("key", DataType::Int16, false),
                        Field::new("value", DataType::Int8, true)
                    ])),
                    true
                )
                .into(),
                true
            )
        ));

        // Test structs

        let struct_field = DataType::Struct(Fields::from(vec![
            Field::new("a", DataType::Int8, true),
            Field::new("b", DataType::Int8, true),
        ]));

        // Succeeds if both have same names and datatypes, ignores nullability
        assert!(DFSchema::datatype_is_logically_equal(
            &struct_field,
            &DataType::Struct(Fields::from(vec![
                Field::new("a", DataType::Int8, false),
                Field::new("b", DataType::Int8, true),
            ]))
        ));

        // Fails if field names are different
        assert!(!DFSchema::datatype_is_logically_equal(
            &struct_field,
            &DataType::Struct(Fields::from(vec![
                Field::new("x", DataType::Int8, true),
                Field::new("y", DataType::Int8, true),
            ]))
        ));

        // Fails if types are different
        assert!(!DFSchema::datatype_is_logically_equal(
            &struct_field,
            &DataType::Struct(Fields::from(vec![
                Field::new("a", DataType::Int16, true),
                Field::new("b", DataType::Int8, true),
            ]))
        ));

        // Fails if more or less fields
        assert!(!DFSchema::datatype_is_logically_equal(
            &struct_field,
            &DataType::Struct(Fields::from(vec![Field::new("a", DataType::Int8, true),]))
        ));
    }

    #[test]
    fn test_datatype_is_logically_equivalent_to_dictionary() {
        // Dictionary is logically equal to its value type
        assert!(DFSchema::datatype_is_logically_equal(
            &DataType::Utf8,
            &DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8))
        ));
    }

    #[test]
    fn test_datatype_is_semantically_equal() {
        assert!(DFSchema::datatype_is_semantically_equal(
            &DataType::Int8,
            &DataType::Int8
        ));

        assert!(!DFSchema::datatype_is_semantically_equal(
            &DataType::Int8,
            &DataType::Int16
        ));

        // Test lists

        // Succeeds if both have the same element type, disregards names and nullability
        assert!(DFSchema::datatype_is_semantically_equal(
            &DataType::List(Field::new_list_field(DataType::Int8, true).into()),
            &DataType::List(Field::new("element", DataType::Int8, false).into())
        ));

        // Fails if element type is different
        assert!(!DFSchema::datatype_is_semantically_equal(
            &DataType::List(Field::new_list_field(DataType::Int8, true).into()),
            &DataType::List(Field::new_list_field(DataType::Int16, true).into())
        ));

        // Test maps
        let map_field = DataType::Map(
            Field::new(
                "entries",
                DataType::Struct(Fields::from(vec![
                    Field::new("key", DataType::Int8, false),
                    Field::new("value", DataType::Int8, true),
                ])),
                true,
            )
            .into(),
            true,
        );

        // Succeeds if both maps have the same key and value types, disregards names and nullability
        assert!(DFSchema::datatype_is_semantically_equal(
            &map_field,
            &DataType::Map(
                Field::new(
                    "pairs",
                    DataType::Struct(Fields::from(vec![
                        Field::new("one", DataType::Int8, false),
                        Field::new("two", DataType::Int8, false)
                    ])),
                    true
                )
                .into(),
                true
            )
        ));
        // Fails if value type is different
        assert!(!DFSchema::datatype_is_semantically_equal(
            &map_field,
            &DataType::Map(
                Field::new(
                    "entries",
                    DataType::Struct(Fields::from(vec![
                        Field::new("key", DataType::Int8, false),
                        Field::new("value", DataType::Int16, true)
                    ])),
                    true
                )
                .into(),
                true
            )
        ));

        // Fails if key type is different
        assert!(!DFSchema::datatype_is_semantically_equal(
            &map_field,
            &DataType::Map(
                Field::new(
                    "entries",
                    DataType::Struct(Fields::from(vec![
                        Field::new("key", DataType::Int16, false),
                        Field::new("value", DataType::Int8, true)
                    ])),
                    true
                )
                .into(),
                true
            )
        ));

        // Test structs

        let struct_field = DataType::Struct(Fields::from(vec![
            Field::new("a", DataType::Int8, true),
            Field::new("b", DataType::Int8, true),
        ]));

        // Succeeds if both have same names and datatypes, ignores nullability
        assert!(DFSchema::datatype_is_logically_equal(
            &struct_field,
            &DataType::Struct(Fields::from(vec![
                Field::new("a", DataType::Int8, false),
                Field::new("b", DataType::Int8, true),
            ]))
        ));

        // Fails if field names are different
        assert!(!DFSchema::datatype_is_logically_equal(
            &struct_field,
            &DataType::Struct(Fields::from(vec![
                Field::new("x", DataType::Int8, true),
                Field::new("y", DataType::Int8, true),
            ]))
        ));

        // Fails if types are different
        assert!(!DFSchema::datatype_is_logically_equal(
            &struct_field,
            &DataType::Struct(Fields::from(vec![
                Field::new("a", DataType::Int16, true),
                Field::new("b", DataType::Int8, true),
            ]))
        ));

        // Fails if more or less fields
        assert!(!DFSchema::datatype_is_logically_equal(
            &struct_field,
            &DataType::Struct(Fields::from(vec![Field::new("a", DataType::Int8, true),]))
        ));
    }

    #[test]
    fn test_datatype_is_not_semantically_equivalent_to_dictionary() {
        // Dictionary is not semantically equal to its value type
        assert!(!DFSchema::datatype_is_semantically_equal(
            &DataType::Utf8,
            &DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8))
        ));
    }

    fn test_schema_2() -> Schema {
        Schema::new(vec![
            Field::new("c100", DataType::Boolean, true),
            Field::new("c101", DataType::Boolean, true),
        ])
    }

    fn test_metadata() -> HashMap<String, String> {
        test_metadata_n(2)
    }

    fn test_metadata_n(n: usize) -> HashMap<String, String> {
        (0..n).map(|i| (format!("k{i}"), format!("v{i}"))).collect()
    }
}
