use std::fmt;
use std::sync::Arc;

use pyo3::types::{PyDict, PyString};
use pyo3::{prelude::*, IntoPyObjectExt, PyTraverseError, PyVisit};

use crate::errors::{ErrorType, LocItem, ValError, ValResult};
use crate::input::{BorrowInput, GenericIterator, Input};
use crate::py_gc::PyGcTraverse;
use crate::recursion_guard::RecursionState;
use crate::tools::SchemaDict;
use crate::ValidationError;

use super::list::get_items_schema;
use super::{
    BuildValidator, CombinedValidator, DefinitionsBuilder, Exactness, Extra, InputType, ValidationState, Validator,
};

#[derive(Debug, Clone)]
pub struct GeneratorValidator {
    item_validator: Option<Arc<CombinedValidator>>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    name: String,
    hide_input_in_errors: bool,
    validation_error_cause: bool,
}

impl BuildValidator for GeneratorValidator {
    const EXPECTED_TYPE: &'static str = "generator";

    fn build(
        schema: &Bound<'_, PyDict>,
        config: Option<&Bound<'_, PyDict>>,
        definitions: &mut DefinitionsBuilder<CombinedValidator>,
    ) -> PyResult<CombinedValidator> {
        let item_validator = get_items_schema(schema, config, definitions)?.map(Arc::new);
        let name = match item_validator {
            Some(ref v) => format!("{}[{}]", Self::EXPECTED_TYPE, v.get_name()),
            None => format!("{}[any]", Self::EXPECTED_TYPE),
        };
        let hide_input_in_errors: bool = config
            .get_as(pyo3::intern!(schema.py(), "hide_input_in_errors"))?
            .unwrap_or(false);
        let validation_error_cause: bool = config
            .get_as(pyo3::intern!(schema.py(), "validation_error_cause"))?
            .unwrap_or(false);
        Ok(Self {
            item_validator,
            name,
            min_length: schema.get_as(pyo3::intern!(schema.py(), "min_length"))?,
            max_length: schema.get_as(pyo3::intern!(schema.py(), "max_length"))?,
            hide_input_in_errors,
            validation_error_cause,
        }
        .into())
    }
}

impl_py_gc_traverse!(GeneratorValidator { item_validator });

impl Validator for GeneratorValidator {
    fn validate<'py>(
        &self,
        py: Python<'py>,
        input: &(impl Input<'py> + ?Sized),
        state: &mut ValidationState<'_, 'py>,
    ) -> ValResult<PyObject> {
        // this validator does not yet support partial validation, disable it to avoid incorrect results
        state.allow_partial = false.into();

        let iterator = input.validate_iter()?.into_static();
        let validator = self.item_validator.as_ref().map(|v| {
            InternalValidator::new(
                "ValidatorIterator",
                v.clone(),
                state,
                self.hide_input_in_errors,
                self.validation_error_cause,
            )
        });

        let v_iterator = ValidatorIterator {
            iterator,
            validator,
            min_length: self.min_length,
            max_length: self.max_length,
            hide_input_in_errors: self.hide_input_in_errors,
            validation_error_cause: self.validation_error_cause,
        };
        Ok(v_iterator.into_py_any(py)?)
    }

    fn get_name(&self) -> &str {
        &self.name
    }
}

#[pyclass(module = "pydantic_core._pydantic_core")]
#[derive(Debug)]
struct ValidatorIterator {
    iterator: GenericIterator<'static>,
    validator: Option<InternalValidator>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    hide_input_in_errors: bool,
    validation_error_cause: bool,
}

#[pymethods]
impl ValidatorIterator {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(mut slf: PyRefMut<'_, Self>, py: Python) -> PyResult<Option<PyObject>> {
        let min_length = slf.min_length;
        let max_length = slf.max_length;
        let hide_input_in_errors = slf.hide_input_in_errors;
        let validation_error_cause = slf.validation_error_cause;
        let Self {
            validator, iterator, ..
        } = &mut *slf;
        macro_rules! next {
            ($iter:ident) => {
                match $iter.next(py)? {
                    Some((next, index)) => match validator {
                        Some(validator) => {
                            if let Some(max_length) = max_length {
                                if index >= max_length {
                                    let val_error = ValError::new_custom_input(
                                        ErrorType::TooLong {
                                            field_type: "Generator".to_string(),
                                            max_length,
                                            actual_length: None,
                                            context: None,
                                        },
                                        $iter.input_as_error_value(py),
                                    );
                                    return Err(ValidationError::from_val_error(
                                        py,
                                        "ValidatorIterator".into_pyobject(py)?.into(),
                                        InputType::Python,
                                        val_error,
                                        None,
                                        hide_input_in_errors,
                                        validation_error_cause,
                                    ));
                                }
                            }
                            validator
                                .validate(py, next.borrow_input(), Some(index.into()))
                                .map(Some)
                        }
                        None => Ok(Some(next.into_pyobject(py)?.unbind())),
                    },
                    None => {
                        if let Some(min_length) = min_length {
                            if $iter.index() < min_length {
                                let val_error = ValError::new_custom_input(
                                    ErrorType::TooShort {
                                        field_type: "Generator".to_string(),
                                        min_length,
                                        actual_length: $iter.index(),
                                        context: None,
                                    },
                                    $iter.input_as_error_value(py),
                                );
                                return Err(ValidationError::from_val_error(
                                    py,
                                    "ValidatorIterator".into_pyobject(py)?.into(),
                                    InputType::Python,
                                    val_error,
                                    None,
                                    hide_input_in_errors,
                                    validation_error_cause,
                                ));
                            }
                        }
                        Ok(None)
                    }
                }
            };
        }

        match iterator {
            GenericIterator::PyIterator(ref mut iter) => next!(iter),
            GenericIterator::JsonArray(ref mut iter) => next!(iter),
        }
    }

    #[getter]
    fn index(&self) -> usize {
        match self.iterator {
            GenericIterator::PyIterator(ref iter) => iter.index(),
            GenericIterator::JsonArray(ref iter) => iter.index(),
        }
    }

    fn __repr__(&self) -> String {
        format!("ValidatorIterator(index={}, schema={:?})", self.index(), self.validator)
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }

    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        self.iterator.py_gc_traverse(&visit)?;
        self.validator.py_gc_traverse(&visit)?;
        Ok(())
    }
}

/// Owned validator wrapper for use in generators in functions, this can be passed back to python
/// mid-validation
pub struct InternalValidator {
    name: String,
    validator: Arc<CombinedValidator>,
    // TODO, do we need data?
    data: Option<Py<PyDict>>,
    strict: Option<bool>,
    from_attributes: Option<bool>,
    context: Option<PyObject>,
    self_instance: Option<PyObject>,
    recursion_guard: RecursionState,
    pub(crate) exactness: Option<Exactness>,
    pub(crate) fields_set_count: Option<usize>,
    validation_mode: InputType,
    hide_input_in_errors: bool,
    validation_error_cause: bool,
    cache_str: jiter::StringCacheMode,
}

impl fmt::Debug for InternalValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.validator)
    }
}

impl InternalValidator {
    pub fn new(
        name: &str,
        validator: Arc<CombinedValidator>,
        state: &ValidationState,
        hide_input_in_errors: bool,
        validation_error_cause: bool,
    ) -> Self {
        let extra = state.extra();
        Self {
            name: name.to_string(),
            validator,
            data: extra.data.as_ref().map(|d| d.clone().into()),
            strict: extra.strict,
            from_attributes: extra.from_attributes,
            context: extra.context.map(|d| d.clone().unbind()),
            self_instance: extra.self_instance.map(|d| d.clone().unbind()),
            recursion_guard: state.recursion_guard.clone(),
            exactness: state.exactness,
            fields_set_count: state.fields_set_count,
            validation_mode: extra.input_type,
            hide_input_in_errors,
            validation_error_cause,
            cache_str: extra.cache_str,
        }
    }

    pub fn validate_assignment<'py>(
        &mut self,
        py: Python<'py>,
        model: &Bound<'py, PyAny>,
        field_name: &str,
        field_value: &Bound<'py, PyAny>,
        outer_location: Option<LocItem>,
    ) -> PyResult<PyObject> {
        let extra = Extra {
            input_type: self.validation_mode,
            data: self.data.as_ref().map(|data| data.bind(py).clone()),
            strict: self.strict,
            from_attributes: self.from_attributes,
            field_name: Some(PyString::new(py, field_name)),
            context: self.context.as_ref().map(|data| data.bind(py)),
            self_instance: self.self_instance.as_ref().map(|data| data.bind(py)),
            cache_str: self.cache_str,
            by_alias: None,
            by_name: None,
        };
        let mut state = ValidationState::new(extra, &mut self.recursion_guard, false.into());
        state.exactness = self.exactness;
        let result = self
            .validator
            .validate_assignment(py, model, field_name, field_value, &mut state)
            .map_err(|e| {
                ValidationError::from_val_error(
                    py,
                    PyString::new(py, &self.name).into(),
                    InputType::Python,
                    e,
                    outer_location,
                    self.hide_input_in_errors,
                    self.validation_error_cause,
                )
            });
        self.exactness = state.exactness;
        result
    }

    pub fn validate<'py>(
        &mut self,
        py: Python<'py>,
        input: &(impl Input<'py> + ?Sized),
        outer_location: Option<LocItem>,
    ) -> PyResult<PyObject> {
        let extra = Extra {
            input_type: self.validation_mode,
            data: self.data.as_ref().map(|data| data.bind(py).clone()),
            strict: self.strict,
            from_attributes: self.from_attributes,
            field_name: None,
            context: self.context.as_ref().map(|data| data.bind(py)),
            self_instance: self.self_instance.as_ref().map(|data| data.bind(py)),
            cache_str: self.cache_str,
            by_alias: None,
            by_name: None,
        };
        let mut state = ValidationState::new(extra, &mut self.recursion_guard, false.into());
        state.exactness = self.exactness;
        state.fields_set_count = self.fields_set_count;
        let result = self.validator.validate(py, input, &mut state).map_err(|e| {
            ValidationError::from_val_error(
                py,
                PyString::new(py, &self.name).into(),
                InputType::Python,
                e,
                outer_location,
                self.hide_input_in_errors,
                self.validation_error_cause,
            )
        });
        self.exactness = state.exactness;
        self.fields_set_count = state.fields_set_count;
        result
    }
}

impl_py_gc_traverse!(InternalValidator {
    validator,
    data,
    context,
    self_instance
});
