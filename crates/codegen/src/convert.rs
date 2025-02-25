use cranelift::prelude::{types, AbiParam};
use cranelift_module::Module;
use hir_ty::Ty;
use internment::Intern;
use rustc_hash::FxHashMap;

use crate::{compiler::MetaTyData, CraneliftSignature};

#[derive(Clone, Copy)]
pub(crate) enum CompType {
    Number(NumberType),
    Pointer(types::Type),
    Void,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NumberType {
    pub(crate) ty: types::Type,
    pub(crate) float: bool,
    pub(crate) signed: bool,
}

impl NumberType {
    pub(crate) fn bit_width(&self) -> u8 {
        self.ty.bits() as u8
    }

    pub(crate) fn max(&self, other: NumberType) -> NumberType {
        let max_bit_width = self.bit_width().max(other.bit_width());

        let max_ty = match (self.float || other.float, max_bit_width) {
            (true, 32) => types::F32,
            (true, 64) => types::F64,
            (true, _) => unreachable!(),
            (false, 8) => types::I8,
            (false, 16) => types::I16,
            (false, 32) => types::I32,
            (false, 64) => types::I64,
            (false, 128) => types::I128,
            (false, _) => unreachable!(),
        };

        NumberType {
            ty: max_ty,
            float: self.float || other.float,
            signed: self.signed || other.signed,
        }
    }
}

impl CompType {
    #[allow(unused)]
    pub(crate) fn is_number_type(&self) -> bool {
        matches!(self, CompType::Number(_))
    }

    pub(crate) fn is_pointer_type(&self) -> bool {
        matches!(self, CompType::Pointer(_))
    }

    #[allow(unused)]
    pub(crate) fn is_void_type(&self) -> bool {
        matches!(self, CompType::Void)
    }

    pub(crate) fn into_real_type(self) -> Option<types::Type> {
        match self {
            CompType::Number(NumberType { ty, .. }) => Some(ty),
            CompType::Pointer(ty) => Some(ty),
            _ => None,
        }
    }

    pub(crate) fn into_number_type(self) -> Option<NumberType> {
        match self {
            CompType::Number(number_ty) => Some(number_ty),
            _ => None,
        }
    }
}

pub(crate) trait ToCompType {
    fn to_comp_type(&self, pointer_ty: types::Type) -> CompType;
}

impl ToCompType for Ty {
    fn to_comp_type(&self, pointer_ty: types::Type) -> CompType {
        match self {
            hir_ty::Ty::NotYetResolved | hir_ty::Ty::Unknown => unreachable!(),
            hir_ty::Ty::IInt(bit_width) | hir_ty::Ty::UInt(bit_width) => {
                let signed = matches!(self, hir_ty::Ty::IInt(_));

                match *bit_width {
                    u32::MAX => CompType::Number(NumberType {
                        ty: pointer_ty,
                        float: false,
                        signed,
                    }),
                    0 => CompType::Number(NumberType {
                        ty: types::I32,
                        float: false,
                        signed,
                    }),
                    8 => CompType::Number(NumberType {
                        ty: types::I8,
                        float: false,
                        signed,
                    }),
                    16 => CompType::Number(NumberType {
                        ty: types::I16,
                        float: false,
                        signed,
                    }),
                    32 => CompType::Number(NumberType {
                        ty: types::I32,
                        float: false,
                        signed,
                    }),
                    64 => CompType::Number(NumberType {
                        ty: types::I64,
                        float: false,
                        signed,
                    }),
                    128 => CompType::Number(NumberType {
                        ty: types::I128,
                        float: false,
                        signed,
                    }),
                    _ => unreachable!(),
                }
            }
            hir_ty::Ty::Float(bit_width) => match bit_width {
                0 => CompType::Number(NumberType {
                    ty: types::F32,
                    float: true,
                    signed: true,
                }),
                32 => CompType::Number(NumberType {
                    ty: types::F32,
                    float: true,
                    signed: true,
                }),
                64 => CompType::Number(NumberType {
                    ty: types::F64,
                    float: true,
                    signed: true,
                }),
                _ => unreachable!(),
            },
            hir_ty::Ty::Bool | hir_ty::Ty::Char => CompType::Number(NumberType {
                ty: types::I8,
                float: false,
                signed: false,
            }),
            hir_ty::Ty::String => CompType::Pointer(pointer_ty),
            hir_ty::Ty::Array { .. } => CompType::Pointer(pointer_ty),
            hir_ty::Ty::Pointer { .. } => CompType::Pointer(pointer_ty),
            hir_ty::Ty::Distinct { ty, .. } => ty.to_comp_type(pointer_ty),
            hir_ty::Ty::Function { .. } => CompType::Pointer(pointer_ty),
            hir_ty::Ty::Struct { .. } => CompType::Pointer(pointer_ty),
            hir_ty::Ty::Type => CompType::Number(NumberType {
                ty: types::I32,
                float: false,
                signed: false,
            }),
            // you should never be able to get an any value
            hir_ty::Ty::Any => CompType::Void,
            hir_ty::Ty::Void => CompType::Void,
            hir_ty::Ty::File(_) => CompType::Void,
        }
    }
}

pub(crate) trait ToCraneliftSignature {
    fn to_cranelift_signature(
        &self,
        module: &dyn Module,
        pointer_ty: types::Type,
    ) -> (CraneliftSignature, FxHashMap<u64, u64>);
}

impl ToCraneliftSignature for (&Vec<Intern<Ty>>, Intern<Ty>) {
    fn to_cranelift_signature(
        &self,
        module: &dyn Module,
        pointer_ty: types::Type,
    ) -> (CraneliftSignature, FxHashMap<u64, u64>) {
        let (param_tys, return_ty) = self;

        let mut real_ty_count = 0;

        let mut new_idx_to_old_idx = FxHashMap::default();

        let mut param_types = param_tys
            .iter()
            .enumerate()
            .filter_map(|(idx, param_ty)| {
                let param_ty = match param_ty.as_ref() {
                    hir_ty::Ty::Void { .. } => None,
                    other_ty => Some(AbiParam::new(
                        other_ty.to_comp_type(pointer_ty).into_real_type().unwrap(),
                    )),
                };
                if param_ty.is_some() {
                    new_idx_to_old_idx.insert(real_ty_count, idx as u64);
                    real_ty_count += 1;
                }
                param_ty
            })
            .collect::<Vec<_>>();

        if return_ty.is_aggregate() {
            // if the callee is expected to return an array,
            // the caller function must supply a memory address
            // to store it in
            param_types.push(AbiParam::new(pointer_ty));
        }

        (
            CraneliftSignature {
                params: param_types,
                returns: return_ty
                    .to_comp_type(pointer_ty)
                    .into_real_type()
                    .map(|ty| vec![AbiParam::new(ty)])
                    .unwrap_or_default(),
                call_conv: module.target_config().default_call_conv,
            },
            new_idx_to_old_idx,
        )
    }
}

pub(crate) const VOID_DISCRIMINANT: u32 = 1;
pub(crate) const INT_DISCRIMINANT: u32 = 2;
pub(crate) const FLOAT_DISCRIMINANT: u32 = 3;
pub(crate) const BOOL_DISCRIMINANT: u32 = 4;
pub(crate) const STRING_DISCRIMINANT: u32 = 5;
pub(crate) const CHAR_DISCRIMINANT: u32 = 6;
pub(crate) const META_TYPE_DISCRIMINANT: u32 = 7;
pub(crate) const ANY_DISCRIMINANT: u32 = 8;
pub(crate) const FILE_DISCRIMINANT: u32 = 9;

pub(crate) const FIRST_COMPLEX_DISCRIMINANT: u32 = 10;

pub(crate) const STRUCT_DISCRIMINANT: u32 = 10;
pub(crate) const DISTINCT_DISCRIMINANT: u32 = 11;
pub(crate) const ARRAY_DISCRIMINANT: u32 = 12;
pub(crate) const POINTER_DISCRIMINANT: u32 = 13;
pub(crate) const FUNCTION_DISCRIMINANT: u32 = 14;

fn simple_id(discriminant: u32, bit_width: u32, signed: bool) -> u32 {
    // the last 6 bits are reserved for the discriminant
    let id = discriminant << 26;

    let byte_width = bit_width / 8;

    let align = byte_width.min(8).max(1) << 5;

    let sign = (signed as u32) << 9;

    id | sign | align | byte_width
}

pub(crate) trait ToTyId {
    fn to_type_id(self, meta_tys: &mut MetaTyData, pointer_ty: types::Type) -> u32;
    fn to_previous_type_id(self, meta_tys: &MetaTyData, pointer_ty: types::Type) -> u32;
}

impl ToTyId for Intern<Ty> {
    fn to_type_id(self, meta_tys: &mut MetaTyData, pointer_ty: types::Type) -> u32 {
        let id = match self.as_ref() {
            Ty::NotYetResolved | Ty::Unknown => unreachable!(),
            Ty::IInt(bit_width) => simple_id(
                INT_DISCRIMINANT,
                match *bit_width {
                    u32::MAX => pointer_ty.bits(),
                    other => other,
                },
                true,
            ),
            Ty::UInt(bit_width) => simple_id(
                INT_DISCRIMINANT,
                match *bit_width {
                    u32::MAX => pointer_ty.bits(),
                    other => other,
                },
                false,
            ),
            Ty::Float(bit_width) => simple_id(FLOAT_DISCRIMINANT, *bit_width, false),
            Ty::Bool => simple_id(BOOL_DISCRIMINANT, 8, false),
            Ty::String => simple_id(STRING_DISCRIMINANT, pointer_ty.bits(), false),
            Ty::Char => simple_id(CHAR_DISCRIMINANT, 8, false),
            Ty::Type => simple_id(META_TYPE_DISCRIMINANT, 32, false),
            Ty::Any => simple_id(ANY_DISCRIMINANT, 0, false),
            Ty::File(_) => simple_id(FILE_DISCRIMINANT, 0, false),
            Ty::Void => simple_id(VOID_DISCRIMINANT, 0, false),
            Ty::Array { sub_ty, .. } => {
                let id = ARRAY_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Array { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap_or_else(|| {
                        meta_tys.tys_to_compile.push(self);
                        meta_tys.array_uid_gen.generate_unique_id()
                    });

                // make sure to compile the sub type too
                sub_ty.to_type_id(meta_tys, pointer_ty);

                return id | list_id;
            }
            Ty::Pointer { sub_ty, .. } => {
                let id = POINTER_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Pointer { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap_or_else(|| {
                        meta_tys.tys_to_compile.push(self);
                        meta_tys.pointer_uid_gen.generate_unique_id()
                    });

                // make sure to compile the sub type too
                sub_ty.to_type_id(meta_tys, pointer_ty);

                return id | list_id;
            }
            Ty::Distinct { ty, .. } => {
                let id = DISTINCT_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Distinct { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap_or_else(|| {
                        meta_tys.tys_to_compile.push(self);
                        meta_tys.distinct_uid_gen.generate_unique_id()
                    });

                // make sure to compile the sub type too
                ty.to_type_id(meta_tys, pointer_ty);

                return id | list_id;
            }
            Ty::Function { .. } => {
                let id = FUNCTION_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Function { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap_or_else(|| {
                        meta_tys.tys_to_compile.push(self);
                        meta_tys.function_uid_gen.generate_unique_id()
                    });

                return id | list_id;
            }
            Ty::Struct { .. } => {
                let id = STRUCT_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Struct { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap_or_else(|| {
                        meta_tys.tys_to_compile.push(self);
                        meta_tys.struct_uid_gen.generate_unique_id()
                    });

                return id | list_id;
            }
        };

        if !meta_tys.tys_to_compile.iter().any(|ty| *ty == self) {
            meta_tys.tys_to_compile.push(self);
        }

        id
    }

    fn to_previous_type_id(self, meta_tys: &MetaTyData, pointer_ty: types::Type) -> u32 {
        match self.as_ref() {
            Ty::NotYetResolved | Ty::Unknown => unreachable!(),
            Ty::IInt(bit_width) => simple_id(
                INT_DISCRIMINANT,
                match *bit_width {
                    u32::MAX => pointer_ty.bits(),
                    other => other,
                },
                true,
            ),
            Ty::UInt(bit_width) => simple_id(
                INT_DISCRIMINANT,
                match *bit_width {
                    u32::MAX => pointer_ty.bits(),
                    other => other,
                },
                false,
            ),
            Ty::Float(bit_width) => simple_id(FLOAT_DISCRIMINANT, *bit_width, false),
            Ty::Bool => simple_id(BOOL_DISCRIMINANT, 8, false),
            Ty::String => simple_id(STRING_DISCRIMINANT, pointer_ty.bits(), false),
            Ty::Char => simple_id(CHAR_DISCRIMINANT, 8, false),
            Ty::Type => simple_id(META_TYPE_DISCRIMINANT, 32, false),
            Ty::Any => simple_id(ANY_DISCRIMINANT, 0, false),
            Ty::File(_) => simple_id(FILE_DISCRIMINANT, 0, false),
            Ty::Void => simple_id(VOID_DISCRIMINANT, 0, false),
            Ty::Array { .. } => {
                let id = ARRAY_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Array { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap();

                id | list_id
            }
            Ty::Pointer { .. } => {
                let id = POINTER_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Pointer { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap();

                id | list_id
            }
            Ty::Distinct { .. } => {
                let id = DISTINCT_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Distinct { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap();

                id | list_id
            }
            Ty::Function { .. } => {
                let id = FUNCTION_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Function { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap();

                id | list_id
            }
            Ty::Struct { .. } => {
                let id = STRUCT_DISCRIMINANT << 26;

                let list_id = meta_tys
                    .tys_to_compile
                    .iter()
                    .filter(|ty| matches!(ty.as_ref(), Ty::Struct { .. }))
                    .enumerate()
                    .find(|(_, ty)| **ty == self)
                    .map(|(idx, _)| idx as u32)
                    .unwrap();

                id | list_id
            }
        }
    }
}
