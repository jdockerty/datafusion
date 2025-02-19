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

use super::binary::{binary_numeric_coercion, comparison_coercion};
use crate::{AggregateUDF, ScalarUDF, Signature, TypeSignature, WindowUDF};
use arrow::{
    compute::can_cast_types,
    datatypes::{DataType, TimeUnit},
};
use datafusion_common::{
    exec_err, internal_datafusion_err, internal_err, plan_err,
    types::{LogicalType, NativeType},
    utils::{coerced_fixed_size_list_to_list, list_ndims},
    Result,
};
use datafusion_expr_common::{
    signature::{ArrayFunctionSignature, FIXED_SIZE_LIST_WILDCARD, TIMEZONE_WILDCARD},
    type_coercion::binary::{comparison_coercion_numeric, string_coercion},
};
use std::sync::Arc;

/// Performs type coercion for scalar function arguments.
///
/// Returns the data types to which each argument must be coerced to
/// match `signature`.
///
/// For more details on coercion in general, please see the
/// [`type_coercion`](crate::type_coercion) module.
pub fn data_types_with_scalar_udf(
    current_types: &[DataType],
    func: &ScalarUDF,
) -> Result<Vec<DataType>> {
    let signature = func.signature();

    if current_types.is_empty() {
        if signature.type_signature.supports_zero_argument() {
            return Ok(vec![]);
        } else {
            return plan_err!("{} does not support zero arguments.", func.name());
        }
    }

    let valid_types =
        get_valid_types_with_scalar_udf(&signature.type_signature, current_types, func)?;

    if valid_types
        .iter()
        .any(|data_type| data_type == current_types)
    {
        return Ok(current_types.to_vec());
    }

    try_coerce_types(
        func.name(),
        valid_types,
        current_types,
        &signature.type_signature,
    )
}

/// Performs type coercion for aggregate function arguments.
///
/// Returns the data types to which each argument must be coerced to
/// match `signature`.
///
/// For more details on coercion in general, please see the
/// [`type_coercion`](crate::type_coercion) module.
pub fn data_types_with_aggregate_udf(
    current_types: &[DataType],
    func: &AggregateUDF,
) -> Result<Vec<DataType>> {
    let signature = func.signature();

    if current_types.is_empty() {
        if signature.type_signature.supports_zero_argument() {
            return Ok(vec![]);
        } else {
            return plan_err!("{} does not support zero arguments.", func.name());
        }
    }

    let valid_types = get_valid_types_with_aggregate_udf(
        &signature.type_signature,
        current_types,
        func,
    )?;
    if valid_types
        .iter()
        .any(|data_type| data_type == current_types)
    {
        return Ok(current_types.to_vec());
    }

    try_coerce_types(
        func.name(),
        valid_types,
        current_types,
        &signature.type_signature,
    )
}

/// Performs type coercion for window function arguments.
///
/// Returns the data types to which each argument must be coerced to
/// match `signature`.
///
/// For more details on coercion in general, please see the
/// [`type_coercion`](crate::type_coercion) module.
pub fn data_types_with_window_udf(
    current_types: &[DataType],
    func: &WindowUDF,
) -> Result<Vec<DataType>> {
    let signature = func.signature();

    if current_types.is_empty() {
        if signature.type_signature.supports_zero_argument() {
            return Ok(vec![]);
        } else {
            return plan_err!("{} does not support zero arguments.", func.name());
        }
    }

    let valid_types =
        get_valid_types_with_window_udf(&signature.type_signature, current_types, func)?;
    if valid_types
        .iter()
        .any(|data_type| data_type == current_types)
    {
        return Ok(current_types.to_vec());
    }

    try_coerce_types(
        func.name(),
        valid_types,
        current_types,
        &signature.type_signature,
    )
}

/// Performs type coercion for function arguments.
///
/// Returns the data types to which each argument must be coerced to
/// match `signature`.
///
/// For more details on coercion in general, please see the
/// [`type_coercion`](crate::type_coercion) module.
pub fn data_types(
    function_name: impl AsRef<str>,
    current_types: &[DataType],
    signature: &Signature,
) -> Result<Vec<DataType>> {
    if current_types.is_empty() {
        if signature.type_signature.supports_zero_argument() {
            return Ok(vec![]);
        } else {
            return plan_err!(
                "signature {:?} does not support zero arguments.",
                &signature.type_signature
            );
        }
    }

    let valid_types = get_valid_types(&signature.type_signature, current_types)?;
    if valid_types
        .iter()
        .any(|data_type| data_type == current_types)
    {
        return Ok(current_types.to_vec());
    }

    try_coerce_types(
        function_name,
        valid_types,
        current_types,
        &signature.type_signature,
    )
}

fn is_well_supported_signature(type_signature: &TypeSignature) -> bool {
    if let TypeSignature::OneOf(signatures) = type_signature {
        return signatures.iter().all(is_well_supported_signature);
    }

    matches!(
        type_signature,
        TypeSignature::UserDefined
            | TypeSignature::Numeric(_)
            | TypeSignature::String(_)
            | TypeSignature::Coercible(_)
            | TypeSignature::Any(_)
            | TypeSignature::NullAry
            | TypeSignature::Comparable(_)
    )
}

fn try_coerce_types(
    function_name: impl AsRef<str>,
    valid_types: Vec<Vec<DataType>>,
    current_types: &[DataType],
    type_signature: &TypeSignature,
) -> Result<Vec<DataType>> {
    let mut valid_types = valid_types;

    // Well-supported signature that returns exact valid types.
    if !valid_types.is_empty() && is_well_supported_signature(type_signature) {
        // There may be many valid types if valid signature is OneOf
        // Otherwise, there should be only one valid type
        if !type_signature.is_one_of() {
            assert_eq!(valid_types.len(), 1);
        }

        let valid_types = valid_types.swap_remove(0);
        if let Some(t) = maybe_data_types_without_coercion(&valid_types, current_types) {
            return Ok(t);
        }
    } else {
        // TODO: Deprecate this branch after all signatures are well-supported (aka coercion has happened already)
        // Try and coerce the argument types to match the signature, returning the
        // coerced types from the first matching signature.
        for valid_types in valid_types {
            if let Some(types) = maybe_data_types(&valid_types, current_types) {
                return Ok(types);
            }
        }
    }

    // none possible -> Error
    plan_err!(
        "Failed to coerce arguments to satisfy a call to {} function: coercion from {:?} to the signature {:?} failed.",
        function_name.as_ref(),
        current_types,
        type_signature
    )
}

fn get_valid_types_with_scalar_udf(
    signature: &TypeSignature,
    current_types: &[DataType],
    func: &ScalarUDF,
) -> Result<Vec<Vec<DataType>>> {
    match signature {
        TypeSignature::UserDefined => match func.coerce_types(current_types) {
            Ok(coerced_types) => Ok(vec![coerced_types]),
            Err(e) => exec_err!("User-defined coercion failed with {:?}", e),
        },
        TypeSignature::OneOf(signatures) => {
            let mut res = vec![];
            let mut errors = vec![];
            for sig in signatures {
                match get_valid_types_with_scalar_udf(sig, current_types, func) {
                    Ok(valid_types) => {
                        res.extend(valid_types);
                    }
                    Err(e) => {
                        errors.push(e.to_string());
                    }
                }
            }

            // Every signature failed, return the joined error
            if res.is_empty() {
                internal_err!(
                    "Failed to match any signature, errors: {}",
                    errors.join(",")
                )
            } else {
                Ok(res)
            }
        }
        _ => get_valid_types(signature, current_types),
    }
}

fn get_valid_types_with_aggregate_udf(
    signature: &TypeSignature,
    current_types: &[DataType],
    func: &AggregateUDF,
) -> Result<Vec<Vec<DataType>>> {
    let valid_types = match signature {
        TypeSignature::UserDefined => match func.coerce_types(current_types) {
            Ok(coerced_types) => vec![coerced_types],
            Err(e) => return exec_err!("User-defined coercion failed with {:?}", e),
        },
        TypeSignature::OneOf(signatures) => signatures
            .iter()
            .filter_map(|t| {
                get_valid_types_with_aggregate_udf(t, current_types, func).ok()
            })
            .flatten()
            .collect::<Vec<_>>(),
        _ => get_valid_types(signature, current_types)?,
    };

    Ok(valid_types)
}

fn get_valid_types_with_window_udf(
    signature: &TypeSignature,
    current_types: &[DataType],
    func: &WindowUDF,
) -> Result<Vec<Vec<DataType>>> {
    let valid_types = match signature {
        TypeSignature::UserDefined => match func.coerce_types(current_types) {
            Ok(coerced_types) => vec![coerced_types],
            Err(e) => return exec_err!("User-defined coercion failed with {:?}", e),
        },
        TypeSignature::OneOf(signatures) => signatures
            .iter()
            .filter_map(|t| get_valid_types_with_window_udf(t, current_types, func).ok())
            .flatten()
            .collect::<Vec<_>>(),
        _ => get_valid_types(signature, current_types)?,
    };

    Ok(valid_types)
}

/// Returns a Vec of all possible valid argument types for the given signature.
fn get_valid_types(
    signature: &TypeSignature,
    current_types: &[DataType],
) -> Result<Vec<Vec<DataType>>> {
    fn array_element_and_optional_index(
        current_types: &[DataType],
    ) -> Result<Vec<Vec<DataType>>> {
        // make sure there's 2 or 3 arguments
        if !(current_types.len() == 2 || current_types.len() == 3) {
            return Ok(vec![vec![]]);
        }

        let first_two_types = &current_types[0..2];
        let mut valid_types = array_append_or_prepend_valid_types(first_two_types, true)?;

        // Early return if there are only 2 arguments
        if current_types.len() == 2 {
            return Ok(valid_types);
        }

        let valid_types_with_index = valid_types
            .iter()
            .map(|t| {
                let mut t = t.clone();
                t.push(DataType::Int64);
                t
            })
            .collect::<Vec<_>>();

        valid_types.extend(valid_types_with_index);

        Ok(valid_types)
    }

    fn array_append_or_prepend_valid_types(
        current_types: &[DataType],
        is_append: bool,
    ) -> Result<Vec<Vec<DataType>>> {
        if current_types.len() != 2 {
            return Ok(vec![vec![]]);
        }

        let (array_type, elem_type) = if is_append {
            (&current_types[0], &current_types[1])
        } else {
            (&current_types[1], &current_types[0])
        };

        // We follow Postgres on `array_append(Null, T)`, which is not valid.
        if array_type.eq(&DataType::Null) {
            return Ok(vec![vec![]]);
        }

        // We need to find the coerced base type, mainly for cases like:
        // `array_append(List(null), i64)` -> `List(i64)`
        let array_base_type = datafusion_common::utils::base_type(array_type);
        let elem_base_type = datafusion_common::utils::base_type(elem_type);
        let new_base_type = comparison_coercion(&array_base_type, &elem_base_type);

        let new_base_type = new_base_type.ok_or_else(|| {
            internal_datafusion_err!(
                "Coercion from {array_base_type:?} to {elem_base_type:?} not supported."
            )
        })?;

        let new_array_type = datafusion_common::utils::coerced_type_with_base_type_only(
            array_type,
            &new_base_type,
        );

        match new_array_type {
            DataType::List(ref field)
            | DataType::LargeList(ref field)
            | DataType::FixedSizeList(ref field, _) => {
                let new_elem_type = field.data_type();
                if is_append {
                    Ok(vec![vec![new_array_type.clone(), new_elem_type.clone()]])
                } else {
                    Ok(vec![vec![new_elem_type.to_owned(), new_array_type.clone()]])
                }
            }
            _ => Ok(vec![vec![]]),
        }
    }
    fn array(array_type: &DataType) -> Option<DataType> {
        match array_type {
            DataType::List(_)
            | DataType::LargeList(_)
            | DataType::FixedSizeList(_, _) => {
                let array_type = coerced_fixed_size_list_to_list(array_type);
                Some(array_type)
            }
            _ => None,
        }
    }

    fn function_length_check(length: usize, expected_length: usize) -> Result<()> {
        if length < 1 {
            return plan_err!(
                "The signature expected at least one argument but received {expected_length}"
            );
        }

        if length != expected_length {
            return plan_err!(
                "The signature expected {length} arguments but received {expected_length}"
            );
        }

        Ok(())
    }

    let valid_types = match signature {
        TypeSignature::Variadic(valid_types) => valid_types
            .iter()
            .map(|valid_type| current_types.iter().map(|_| valid_type.clone()).collect())
            .collect(),
        TypeSignature::String(number) => {
            function_length_check(current_types.len(), *number)?;

            let mut new_types = Vec::with_capacity(current_types.len());
            for data_type in current_types.iter() {
                let logical_data_type: NativeType = data_type.into();
                if logical_data_type == NativeType::String {
                    new_types.push(data_type.to_owned());
                } else if logical_data_type == NativeType::Null {
                    // TODO: Switch to Utf8View if all the string functions supports Utf8View
                    new_types.push(DataType::Utf8);
                } else {
                    return plan_err!(
                        "The signature expected NativeType::String but received {logical_data_type}"
                    );
                }
            }

            // Find the common string type for the given types
            fn find_common_type(
                lhs_type: &DataType,
                rhs_type: &DataType,
            ) -> Result<DataType> {
                match (lhs_type, rhs_type) {
                    (DataType::Dictionary(_, lhs), DataType::Dictionary(_, rhs)) => {
                        find_common_type(lhs, rhs)
                    }
                    (DataType::Dictionary(_, v), other)
                    | (other, DataType::Dictionary(_, v)) => find_common_type(v, other),
                    _ => {
                        if let Some(coerced_type) = string_coercion(lhs_type, rhs_type) {
                            Ok(coerced_type)
                        } else {
                            plan_err!(
                                "{} and {} are not coercible to a common string type",
                                lhs_type,
                                rhs_type
                            )
                        }
                    }
                }
            }

            // Length checked above, safe to unwrap
            let mut coerced_type = new_types.first().unwrap().to_owned();
            for t in new_types.iter().skip(1) {
                coerced_type = find_common_type(&coerced_type, t)?;
            }

            fn base_type_or_default_type(data_type: &DataType) -> DataType {
                if let DataType::Dictionary(_, v) = data_type {
                    base_type_or_default_type(v)
                } else {
                    data_type.to_owned()
                }
            }

            vec![vec![base_type_or_default_type(&coerced_type); *number]]
        }
        TypeSignature::Numeric(number) => {
            function_length_check(current_types.len(), *number)?;

            // Find common numeric type amongs given types except string
            let mut valid_type = current_types.first().unwrap().to_owned();
            for t in current_types.iter().skip(1) {
                let logical_data_type: NativeType = t.into();
                if logical_data_type == NativeType::Null {
                    continue;
                }

                if !logical_data_type.is_numeric() {
                    return plan_err!(
                        "The signature expected NativeType::Numeric but received {logical_data_type}"
                    );
                }

                if let Some(coerced_type) = binary_numeric_coercion(&valid_type, t) {
                    valid_type = coerced_type;
                } else {
                    return plan_err!(
                        "{} and {} are not coercible to a common numeric type",
                        valid_type,
                        t
                    );
                }
            }

            let logical_data_type: NativeType = valid_type.clone().into();
            // Fallback to default type if we don't know which type to coerced to
            // f64 is chosen since most of the math functions utilize Signature::numeric,
            // and their default type is double precision
            if logical_data_type == NativeType::Null {
                valid_type = DataType::Float64;
            }

            vec![vec![valid_type; *number]]
        }
        TypeSignature::Comparable(num) => {
            function_length_check(current_types.len(), *num)?;
            let mut target_type = current_types[0].to_owned();
            for data_type in current_types.iter().skip(1) {
                if let Some(dt) = comparison_coercion_numeric(&target_type, data_type) {
                    target_type = dt;
                } else {
                    return plan_err!("{target_type} and {data_type} is not comparable");
                }
            }
            // Convert null to String type.
            if target_type.is_null() {
                vec![vec![DataType::Utf8View; *num]]
            } else {
                vec![vec![target_type; *num]]
            }
        }
        TypeSignature::Coercible(target_types) => {
            function_length_check(current_types.len(), target_types.len())?;

            // Aim to keep this logic as SIMPLE as possible!
            // Make sure the corresponding test is covered
            // If this function becomes COMPLEX, create another new signature!
            fn can_coerce_to(
                logical_type: &NativeType,
                target_type: &NativeType,
            ) -> bool {
                if logical_type == target_type {
                    return true;
                }

                if logical_type == &NativeType::Null {
                    return true;
                }

                if target_type.is_integer() && logical_type.is_integer() {
                    return true;
                }

                false
            }

            let mut new_types = Vec::with_capacity(current_types.len());
            for (current_type, target_type) in
                current_types.iter().zip(target_types.iter())
            {
                let logical_type: NativeType = current_type.into();
                let target_logical_type = target_type.native();
                if can_coerce_to(&logical_type, target_logical_type) {
                    let target_type =
                        target_logical_type.default_cast_for(current_type)?;
                    new_types.push(target_type);
                }
            }

            vec![new_types]
        }
        TypeSignature::Uniform(number, valid_types) => {
            if *number == 0 {
                return plan_err!("The function expected at least one argument");
            }

            valid_types
                .iter()
                .map(|valid_type| (0..*number).map(|_| valid_type.clone()).collect())
                .collect()
        }
        TypeSignature::UserDefined => {
            return internal_err!(
            "User-defined signature should be handled by function-specific coerce_types."
        )
        }
        TypeSignature::VariadicAny => {
            if current_types.is_empty() {
                return plan_err!(
                    "The function expected at least one argument but received 0"
                );
            }
            vec![current_types.to_vec()]
        }
        TypeSignature::Exact(valid_types) => vec![valid_types.clone()],
        TypeSignature::ArraySignature(ref function_signature) => match function_signature
        {
            ArrayFunctionSignature::ArrayAndElement => {
                array_append_or_prepend_valid_types(current_types, true)?
            }
            ArrayFunctionSignature::ElementAndArray => {
                array_append_or_prepend_valid_types(current_types, false)?
            }
            ArrayFunctionSignature::ArrayAndIndex => {
                if current_types.len() != 2 {
                    return Ok(vec![vec![]]);
                }
                array(&current_types[0]).map_or_else(
                    || vec![vec![]],
                    |array_type| vec![vec![array_type, DataType::Int64]],
                )
            }
            ArrayFunctionSignature::ArrayAndElementAndOptionalIndex => {
                array_element_and_optional_index(current_types)?
            }
            ArrayFunctionSignature::Array => {
                if current_types.len() != 1 {
                    return Ok(vec![vec![]]);
                }

                array(&current_types[0])
                    .map_or_else(|| vec![vec![]], |array_type| vec![vec![array_type]])
            }
            ArrayFunctionSignature::MapArray => {
                if current_types.len() != 1 {
                    return Ok(vec![vec![]]);
                }

                match &current_types[0] {
                    DataType::Map(_, _) => vec![vec![current_types[0].clone()]],
                    _ => vec![vec![]],
                }
            }
        },
        TypeSignature::NullAry => {
            if !current_types.is_empty() {
                return plan_err!(
                    "The function expected zero argument but received {}",
                    current_types.len()
                );
            }
            vec![vec![]]
        }
        TypeSignature::Any(number) => {
            if current_types.is_empty() {
                return plan_err!(
                    "The function expected at least one argument but received 0"
                );
            }

            if current_types.len() != *number {
                return plan_err!(
                    "The function expected {} arguments but received {}",
                    number,
                    current_types.len()
                );
            }
            vec![(0..*number).map(|i| current_types[i].clone()).collect()]
        }
        TypeSignature::OneOf(types) => types
            .iter()
            .filter_map(|t| get_valid_types(t, current_types).ok())
            .flatten()
            .collect::<Vec<_>>(),
    };

    Ok(valid_types)
}

/// Try to coerce the current argument types to match the given `valid_types`.
///
/// For example, if a function `func` accepts arguments of  `(int64, int64)`,
/// but was called with `(int32, int64)`, this function could match the
/// valid_types by coercing the first argument to `int64`, and would return
/// `Some([int64, int64])`.
fn maybe_data_types(
    valid_types: &[DataType],
    current_types: &[DataType],
) -> Option<Vec<DataType>> {
    if valid_types.len() != current_types.len() {
        return None;
    }

    let mut new_type = Vec::with_capacity(valid_types.len());
    for (i, valid_type) in valid_types.iter().enumerate() {
        let current_type = &current_types[i];

        if current_type == valid_type {
            new_type.push(current_type.clone())
        } else {
            // attempt to coerce.
            // TODO: Replace with `can_cast_types` after failing cases are resolved
            // (they need new signature that returns exactly valid types instead of list of possible valid types).
            if let Some(coerced_type) = coerced_from(valid_type, current_type) {
                new_type.push(coerced_type)
            } else {
                // not possible
                return None;
            }
        }
    }
    Some(new_type)
}

/// Check if the current argument types can be coerced to match the given `valid_types`
/// unlike `maybe_data_types`, this function does not coerce the types.
/// TODO: I think this function should replace `maybe_data_types` after signature are well-supported.
fn maybe_data_types_without_coercion(
    valid_types: &[DataType],
    current_types: &[DataType],
) -> Option<Vec<DataType>> {
    if valid_types.len() != current_types.len() {
        return None;
    }

    let mut new_type = Vec::with_capacity(valid_types.len());
    for (i, valid_type) in valid_types.iter().enumerate() {
        let current_type = &current_types[i];

        if current_type == valid_type {
            new_type.push(current_type.clone())
        } else if can_cast_types(current_type, valid_type) {
            // validate the valid type is castable from the current type
            new_type.push(valid_type.clone())
        } else {
            return None;
        }
    }
    Some(new_type)
}

/// Return true if a value of type `type_from` can be coerced
/// (losslessly converted) into a value of `type_to`
///
/// See the module level documentation for more detail on coercion.
pub fn can_coerce_from(type_into: &DataType, type_from: &DataType) -> bool {
    if type_into == type_from {
        return true;
    }
    if let Some(coerced) = coerced_from(type_into, type_from) {
        return coerced == *type_into;
    }
    false
}

/// Find the coerced type for the given `type_into` and `type_from`.
/// Returns `None` if coercion is not possible.
///
/// Expect uni-directional coercion, for example, i32 is coerced to i64, but i64 is not coerced to i32.
///
/// Unlike [comparison_coercion], the coerced type is usually `wider` for lossless conversion.
fn coerced_from<'a>(
    type_into: &'a DataType,
    type_from: &'a DataType,
) -> Option<DataType> {
    use self::DataType::*;

    // match Dictionary first
    match (type_into, type_from) {
        // coerced dictionary first
        (_, Dictionary(_, value_type))
            if coerced_from(type_into, value_type).is_some() =>
        {
            Some(type_into.clone())
        }
        (Dictionary(_, value_type), _)
            if coerced_from(value_type, type_from).is_some() =>
        {
            Some(type_into.clone())
        }
        // coerced into type_into
        (Int8, Null | Int8) => Some(type_into.clone()),
        (Int16, Null | Int8 | Int16 | UInt8) => Some(type_into.clone()),
        (Int32, Null | Int8 | Int16 | Int32 | UInt8 | UInt16) => Some(type_into.clone()),
        (Int64, Null | Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32) => {
            Some(type_into.clone())
        }
        (UInt8, Null | UInt8) => Some(type_into.clone()),
        (UInt16, Null | UInt8 | UInt16) => Some(type_into.clone()),
        (UInt32, Null | UInt8 | UInt16 | UInt32) => Some(type_into.clone()),
        (UInt64, Null | UInt8 | UInt16 | UInt32 | UInt64) => Some(type_into.clone()),
        (
            Float32,
            Null | Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64
            | Float32,
        ) => Some(type_into.clone()),
        (
            Float64,
            Null
            | Int8
            | Int16
            | Int32
            | Int64
            | UInt8
            | UInt16
            | UInt32
            | UInt64
            | Float32
            | Float64
            | Decimal128(_, _),
        ) => Some(type_into.clone()),
        (
            Timestamp(TimeUnit::Nanosecond, None),
            Null | Timestamp(_, None) | Date32 | Utf8 | LargeUtf8,
        ) => Some(type_into.clone()),
        (Interval(_), Utf8 | LargeUtf8) => Some(type_into.clone()),
        // We can go into a Utf8View from a Utf8 or LargeUtf8
        (Utf8View, Utf8 | LargeUtf8 | Null) => Some(type_into.clone()),
        // Any type can be coerced into strings
        (Utf8 | LargeUtf8, _) => Some(type_into.clone()),
        (Null, _) if can_cast_types(type_from, type_into) => Some(type_into.clone()),

        (List(_), FixedSizeList(_, _)) => Some(type_into.clone()),

        // Only accept list and largelist with the same number of dimensions unless the type is Null.
        // List or LargeList with different dimensions should be handled in TypeSignature or other places before this
        (List(_) | LargeList(_), _)
            if datafusion_common::utils::base_type(type_from).eq(&Null)
                || list_ndims(type_from) == list_ndims(type_into) =>
        {
            Some(type_into.clone())
        }
        // should be able to coerce wildcard fixed size list to non wildcard fixed size list
        (
            FixedSizeList(f_into, FIXED_SIZE_LIST_WILDCARD),
            FixedSizeList(f_from, size_from),
        ) => match coerced_from(f_into.data_type(), f_from.data_type()) {
            Some(data_type) if &data_type != f_into.data_type() => {
                let new_field =
                    Arc::new(f_into.as_ref().clone().with_data_type(data_type));
                Some(FixedSizeList(new_field, *size_from))
            }
            Some(_) => Some(FixedSizeList(Arc::clone(f_into), *size_from)),
            _ => None,
        },
        (Timestamp(unit, Some(tz)), _) if tz.as_ref() == TIMEZONE_WILDCARD => {
            match type_from {
                Timestamp(_, Some(from_tz)) => {
                    Some(Timestamp(*unit, Some(Arc::clone(from_tz))))
                }
                Null | Date32 | Utf8 | LargeUtf8 | Timestamp(_, None) => {
                    // In the absence of any other information assume the time zone is "+00" (UTC).
                    Some(Timestamp(*unit, Some("+00".into())))
                }
                _ => None,
            }
        }
        (Timestamp(_, Some(_)), Null | Timestamp(_, _) | Date32 | Utf8 | LargeUtf8) => {
            Some(type_into.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {

    use crate::Volatility;

    use super::*;
    use arrow::datatypes::Field;

    #[test]
    fn test_string_conversion() {
        let cases = vec![
            (DataType::Utf8View, DataType::Utf8, true),
            (DataType::Utf8View, DataType::LargeUtf8, true),
        ];

        for case in cases {
            assert_eq!(can_coerce_from(&case.0, &case.1), case.2);
        }
    }

    #[test]
    fn test_maybe_data_types() {
        // this vec contains: arg1, arg2, expected result
        let cases = vec![
            // 2 entries, same values
            (
                vec![DataType::UInt8, DataType::UInt16],
                vec![DataType::UInt8, DataType::UInt16],
                Some(vec![DataType::UInt8, DataType::UInt16]),
            ),
            // 2 entries, can coerce values
            (
                vec![DataType::UInt16, DataType::UInt16],
                vec![DataType::UInt8, DataType::UInt16],
                Some(vec![DataType::UInt16, DataType::UInt16]),
            ),
            // 0 entries, all good
            (vec![], vec![], Some(vec![])),
            // 2 entries, can't coerce
            (
                vec![DataType::Boolean, DataType::UInt16],
                vec![DataType::UInt8, DataType::UInt16],
                None,
            ),
            // u32 -> u16 is possible
            (
                vec![DataType::Boolean, DataType::UInt32],
                vec![DataType::Boolean, DataType::UInt16],
                Some(vec![DataType::Boolean, DataType::UInt32]),
            ),
            // UTF8 -> Timestamp
            (
                vec![
                    DataType::Timestamp(TimeUnit::Nanosecond, None),
                    DataType::Timestamp(TimeUnit::Nanosecond, Some("+TZ".into())),
                    DataType::Timestamp(TimeUnit::Nanosecond, Some("+01".into())),
                ],
                vec![DataType::Utf8, DataType::Utf8, DataType::Utf8],
                Some(vec![
                    DataType::Timestamp(TimeUnit::Nanosecond, None),
                    DataType::Timestamp(TimeUnit::Nanosecond, Some("+00".into())),
                    DataType::Timestamp(TimeUnit::Nanosecond, Some("+01".into())),
                ]),
            ),
        ];

        for case in cases {
            assert_eq!(maybe_data_types(&case.0, &case.1), case.2)
        }
    }

    #[test]
    fn test_get_valid_types_one_of() -> Result<()> {
        let signature =
            TypeSignature::OneOf(vec![TypeSignature::Any(1), TypeSignature::Any(2)]);

        let invalid_types = get_valid_types(
            &signature,
            &[DataType::Int32, DataType::Int32, DataType::Int32],
        )?;
        assert_eq!(invalid_types.len(), 0);

        let args = vec![DataType::Int32, DataType::Int32];
        let valid_types = get_valid_types(&signature, &args)?;
        assert_eq!(valid_types.len(), 1);
        assert_eq!(valid_types[0], args);

        let args = vec![DataType::Int32];
        let valid_types = get_valid_types(&signature, &args)?;
        assert_eq!(valid_types.len(), 1);
        assert_eq!(valid_types[0], args);

        Ok(())
    }

    #[test]
    fn test_fixed_list_wildcard_coerce() -> Result<()> {
        let inner = Arc::new(Field::new_list_field(DataType::Int32, false));
        let current_types = vec![
            DataType::FixedSizeList(Arc::clone(&inner), 2), // able to coerce for any size
        ];

        let signature = Signature::exact(
            vec![DataType::FixedSizeList(
                Arc::clone(&inner),
                FIXED_SIZE_LIST_WILDCARD,
            )],
            Volatility::Stable,
        );

        let coerced_data_types = data_types("test", &current_types, &signature).unwrap();
        assert_eq!(coerced_data_types, current_types);

        // make sure it can't coerce to a different size
        let signature = Signature::exact(
            vec![DataType::FixedSizeList(Arc::clone(&inner), 3)],
            Volatility::Stable,
        );
        let coerced_data_types = data_types("test", &current_types, &signature);
        assert!(coerced_data_types.is_err());

        // make sure it works with the same type.
        let signature = Signature::exact(
            vec![DataType::FixedSizeList(Arc::clone(&inner), 2)],
            Volatility::Stable,
        );
        let coerced_data_types = data_types("test", &current_types, &signature).unwrap();
        assert_eq!(coerced_data_types, current_types);

        Ok(())
    }

    #[test]
    fn test_nested_wildcard_fixed_size_lists() -> Result<()> {
        let type_into = DataType::FixedSizeList(
            Arc::new(Field::new_list_field(
                DataType::FixedSizeList(
                    Arc::new(Field::new_list_field(DataType::Int32, false)),
                    FIXED_SIZE_LIST_WILDCARD,
                ),
                false,
            )),
            FIXED_SIZE_LIST_WILDCARD,
        );

        let type_from = DataType::FixedSizeList(
            Arc::new(Field::new_list_field(
                DataType::FixedSizeList(
                    Arc::new(Field::new_list_field(DataType::Int8, false)),
                    4,
                ),
                false,
            )),
            3,
        );

        assert_eq!(
            coerced_from(&type_into, &type_from),
            Some(DataType::FixedSizeList(
                Arc::new(Field::new_list_field(
                    DataType::FixedSizeList(
                        Arc::new(Field::new_list_field(DataType::Int32, false)),
                        4,
                    ),
                    false,
                )),
                3,
            ))
        );

        Ok(())
    }

    #[test]
    fn test_coerced_from_dictionary() {
        let type_into =
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::UInt32));
        let type_from = DataType::Int64;
        assert_eq!(coerced_from(&type_into, &type_from), None);

        let type_from =
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::UInt32));
        let type_into = DataType::Int64;
        assert_eq!(
            coerced_from(&type_into, &type_from),
            Some(type_into.clone())
        );
    }
}
