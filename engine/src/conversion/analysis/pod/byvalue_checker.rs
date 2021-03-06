// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::known_types::KNOWN_TYPES;
use crate::types::{Namespace, TypeName};
use std::collections::HashMap;
use syn::{ItemStruct, Type};

#[derive(Clone)]
enum PODState {
    UnsafeToBePOD(String),
    SafeToBePOD,
    IsPOD,
    IsAlias(TypeName),
}

#[derive(Clone)]
struct StructDetails {
    state: PODState,
    dependent_structs: Vec<TypeName>,
}

impl StructDetails {
    fn new(state: PODState) -> Self {
        StructDetails {
            state,
            dependent_structs: Vec::new(),
        }
    }
}

/// Type which is able to check whether it's safe to make a type
/// fully representable by cxx. For instance if it is a struct containing
/// a struct containing a std::string, the answer is no, because that
/// std::string contains a self-referential pointer.
pub struct ByValueChecker {
    // Mapping from type name to whether it is safe to be POD
    results: HashMap<TypeName, StructDetails>,
}

impl ByValueChecker {
    pub fn new() -> Self {
        let mut results = HashMap::new();
        for (tn, by_value_safe) in KNOWN_TYPES.get_pod_safe_types() {
            let safety = if by_value_safe {
                PODState::IsPOD
            } else {
                PODState::UnsafeToBePOD(format!("type {} is not safe for POD", tn))
            };
            results.insert(tn, StructDetails::new(safety));
        }
        ByValueChecker { results }
    }

    pub fn ingest_struct(&mut self, def: &ItemStruct, ns: &Namespace) {
        // For this struct, work out whether it _could_ be safe as a POD.
        let tyname = TypeName::new(ns, &def.ident.to_string());
        let mut field_safety_problem = PODState::SafeToBePOD;
        let fieldlist = Self::get_field_types(def);
        for ty_id in &fieldlist {
            match self.results.get(ty_id) {
                None => {
                    field_safety_problem = PODState::UnsafeToBePOD(format!(
                        "Type {} could not be POD because its dependent type {} isn't known",
                        tyname, ty_id
                    ));
                    break;
                }
                Some(deets) => {
                    if let PODState::UnsafeToBePOD(reason) = &deets.state {
                        let new_reason = format!("Type {} could not be POD because its dependent type {} isn't safe to be POD. Because: {}", tyname, ty_id, reason);
                        field_safety_problem = PODState::UnsafeToBePOD(new_reason);
                        break;
                    }
                }
            }
        }
        let has_vtable = Self::has_vtable(def);
        if has_vtable {
            let reason = format!(
                "Type {} could not be POD because it has virtual functions.",
                tyname
            );
            field_safety_problem = PODState::UnsafeToBePOD(reason);
        }
        let mut my_details = StructDetails::new(field_safety_problem);
        my_details.dependent_structs = fieldlist;
        self.results.insert(tyname, my_details);
    }

    pub fn ingest_pod_type(&mut self, tyname: TypeName) {
        self.results
            .insert(tyname, StructDetails::new(PODState::IsPOD));
    }

    pub fn ingest_simple_typedef(&mut self, tyname: TypeName, target: TypeName) {
        self.results
            .insert(tyname, StructDetails::new(PODState::IsAlias(target)));
    }

    pub fn ingest_nonpod_type(&mut self, tyname: TypeName) {
        let new_reason = format!("Type {} is a typedef to a complex type", tyname);
        self.results.insert(
            tyname,
            StructDetails::new(PODState::UnsafeToBePOD(new_reason)),
        );
    }

    pub fn satisfy_requests(&mut self, mut requests: Vec<TypeName>) -> Result<(), String> {
        while !requests.is_empty() {
            let ty_id = requests.remove(requests.len() - 1);
            let deets = self.results.get_mut(&ty_id);
            let mut alias_to_consider = None;
            match deets {
                None => {
                    return Err(format!(
                        "Unable to make {} POD because we never saw a struct definition",
                        ty_id
                    ))
                }
                Some(deets) => match &deets.state {
                    PODState::UnsafeToBePOD(error_msg) => return Err(error_msg.clone()),
                    PODState::IsPOD => {}
                    PODState::SafeToBePOD => {
                        deets.state = PODState::IsPOD;
                        requests.extend_from_slice(&deets.dependent_structs);
                    }
                    PODState::IsAlias(target_type) => {
                        alias_to_consider = Some(target_type.clone());
                    }
                },
            }
            // Do the following outside the match to avoid borrow checker violation.
            if let Some(alias) = alias_to_consider {
                match self.results.get(&alias) {
                    None => requests.extend_from_slice(&[alias, ty_id]), // try again after resolving alias target
                    Some(alias_target_deets) => {
                        self.results.get_mut(&ty_id).unwrap().state =
                            alias_target_deets.state.clone();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn is_pod(&self, ty_id: &TypeName) -> bool {
        if !ty_id.has_namespace() && ty_id.get_final_ident().starts_with("AutocxxConcrete") {
            // Type we created at conversion time.
            return false;
        }
        matches!(self
        .results
        .get(ty_id)
        .unwrap_or_else(|| panic!("Type {} not known to byvalue_checker", ty_id.to_string())), StructDetails {
            state: PODState::IsPOD,
            dependent_structs: _,
        })
    }

    fn get_field_types(def: &ItemStruct) -> Vec<TypeName> {
        let mut results = Vec::new();
        for f in &def.fields {
            let fty = &f.ty;
            if let Type::Path(p) = fty {
                results.push(TypeName::from_type_path(&p));
            }
            // TODO handle anything else which bindgen might spit out, e.g. arrays?
        }
        results
    }

    fn has_vtable(def: &ItemStruct) -> bool {
        for f in &def.fields {
            if f.ident.as_ref().map(|id| id == "vtable_").unwrap_or(false) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::ByValueChecker;
    use crate::types::{Namespace, TypeName};
    use syn::{parse_quote, ItemStruct};

    #[test]
    fn test_primitive_by_itself() {
        let bvc = ByValueChecker::new();
        let t_id = TypeName::new_from_user_input("u32");
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_primitives() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Foo {
                a: i32,
                b: i64,
            }
        };
        let t_id = TypeName::from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_nested_primitives() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Foo {
                a: i32,
                b: i64,
            }
        };
        bvc.ingest_struct(&t, &Namespace::new());
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: Foo,
                b: i64,
            }
        };
        let t_id = TypeName::from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_with_up() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: UniquePtr<CxxString>,
                b: i64,
            }
        };
        let t_id = TypeName::from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        bvc.satisfy_requests(vec![t_id.clone()]).unwrap();
        assert!(bvc.is_pod(&t_id));
    }

    #[test]
    fn test_with_cxxstring() {
        let mut bvc = ByValueChecker::new();
        let t: ItemStruct = parse_quote! {
            struct Bar {
                a: CxxString,
                b: i64,
            }
        };
        let t_id = TypeName::from_ident(&t.ident);
        bvc.ingest_struct(&t, &Namespace::new());
        assert!(bvc.satisfy_requests(vec![t_id]).is_err());
    }
}
