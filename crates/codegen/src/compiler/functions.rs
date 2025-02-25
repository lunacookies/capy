use std::collections::VecDeque;

use cranelift::{
    codegen::ir::{Endianness, FuncRef, StackSlot},
    prelude::{
        types, Block, EntityRef, FloatCC, FunctionBuilder, InstBuilder, IntCC, MemFlags,
        StackSlotData, StackSlotKind, Value, Variable,
    },
};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use hir::{LocalDef, ScopeId};
use hir_ty::Ty;
use interner::Interner;
use internment::Intern;
use la_arena::Idx;
use rustc_hash::FxHashMap;
use uid_gen::UIDGenerator;

use crate::{
    builtin::BuiltinFunction,
    convert::{NumberType, ToCompType, ToCraneliftSignature, ToTyId},
    mangle::Mangle,
    size::GetMemInfo,
    ComptimeToCompile, CraneliftSignature,
};

use super::{comptime::ComptimeResult, FunctionToCompile, MetaTyData};

pub(crate) struct FunctionCompiler<'a> {
    pub(crate) file_name: hir::FileName,
    pub(crate) signature: CraneliftSignature,

    pub(crate) mod_dir: &'a std::path::Path,
    pub(crate) interner: &'a Interner,
    pub(crate) bodies_map: &'a FxHashMap<hir::FileName, hir::Bodies>,
    pub(crate) tys: &'a hir_ty::InferenceResult,

    pub(crate) builder: FunctionBuilder<'a>,
    pub(crate) module: &'a mut dyn Module,
    pub(crate) data_description: &'a mut DataDescription,
    pub(crate) pointer_ty: types::Type,

    pub(crate) functions_to_compile: &'a mut VecDeque<FunctionToCompile>,
    pub(crate) meta_tys: &'a mut MetaTyData,

    pub(crate) local_functions: FxHashMap<hir::Fqn, FuncRef>,
    pub(crate) local_lambdas: FxHashMap<Idx<hir::Lambda>, FuncRef>,

    // globals
    pub(crate) functions: &'a mut FxHashMap<hir::Fqn, FuncId>,
    pub(crate) compiler_defined_functions: &'a mut FxHashMap<BuiltinFunction, FuncId>,
    pub(crate) globals: &'a mut FxHashMap<hir::Fqn, DataId>,
    pub(crate) str_id_gen: &'a mut UIDGenerator,
    pub(crate) comptime_results: &'a FxHashMap<ComptimeToCompile, ComptimeResult>,

    // variables
    pub(crate) var_id_gen: UIDGenerator,
    pub(crate) locals: FxHashMap<Idx<LocalDef>, Value>,
    pub(crate) params: FxHashMap<u64, Variable>,

    // for control flow (breaks and continues)
    pub(crate) exits: FxHashMap<ScopeId, Block>,
    pub(crate) continues: FxHashMap<ScopeId, Block>,
}

impl FunctionCompiler<'_> {
    pub(crate) fn finish(
        mut self,
        param_tys: Vec<Intern<Ty>>,
        return_ty: Intern<Ty>,
        function_body: Idx<hir::Expr>,
        new_idx_to_old_idx: FxHashMap<u64, u64>,
    ) {
        // Create the entry block, to start emitting code in.
        let entry_block = self.builder.create_block();

        self.builder
            .append_block_params_for_function_params(entry_block);

        self.builder.switch_to_block(entry_block);
        self.builder.seal_block(entry_block);

        let mut dest_param = None;

        for (idx, param) in self.signature.params.iter().enumerate() {
            let param_ty = param.value_type;

            let var = Variable::new(self.var_id_gen.generate_unique_id() as usize);

            if new_idx_to_old_idx.contains_key(&(idx as u64)) {
                self.params.insert(new_idx_to_old_idx[&(idx as u64)], var);
            } else {
                let old_dest_param = dest_param.replace(var);
                assert!(old_dest_param.is_none());
            }

            self.builder.declare_var(var, param_ty);

            let value = self.builder.block_params(entry_block)[idx];

            let old_idx = match new_idx_to_old_idx.get(&(idx as u64)) {
                Some(old_idx) => *old_idx,
                None => {
                    self.builder.def_var(var, value);
                    continue;
                }
            };

            let param_ty = param_tys[old_idx as usize];
            if param_ty.is_aggregate() {
                let size = param_ty.size();

                let stack_slot = self.builder.create_sized_stack_slot(StackSlotData {
                    kind: StackSlotKind::ExplicitSlot,
                    size,
                });

                let stack_slot_addr = self
                    .builder
                    .ins()
                    .stack_addr(self.pointer_ty, stack_slot, 0);

                let size = self.builder.ins().iconst(self.pointer_ty, size as i64);

                self.builder
                    .call_memcpy(self.module.target_config(), stack_slot_addr, value, size);

                self.builder.def_var(var, stack_slot_addr);
            } else {
                self.builder.def_var(var, value);
            }
        }

        // let hir_body = self.bodies_map[&self.module_name].function_body(self.module_name.name);

        match self.compile_expr(function_body) {
            Some(body) => {
                if return_ty.is_aggregate() {
                    let dest = self.builder.use_var(dest_param.unwrap());

                    let aggregate_size = return_ty.size();
                    let aggregate_size = self
                        .builder
                        .ins()
                        .iconst(self.pointer_ty, aggregate_size as i64);

                    self.builder.call_memcpy(
                        self.module.target_config(),
                        dest,
                        body,
                        aggregate_size,
                    );

                    self.builder.ins().return_(&[dest])
                } else if let Some(return_ty) =
                    return_ty.to_comp_type(self.pointer_ty).into_number_type()
                {
                    // the actual type that was returned might not be what the function was
                    // actually supposed to return, so we have to cast it to make sure
                    let body_ty = self.tys[self.file_name][function_body]
                        .to_comp_type(self.pointer_ty)
                        .into_number_type()
                        .unwrap();

                    let cast = super::cast(&mut self.builder, body, body_ty, return_ty);

                    self.builder.ins().return_(&[cast])
                } else {
                    self.builder.ins().return_(&[body])
                }
            }
            None => self.builder.ins().return_(&[]),
        };

        self.builder.seal_all_blocks();
        self.builder.finalize();
    }

    fn expr_to_const_data(&mut self, module: hir::FileName, expr: Idx<hir::Expr>) -> Box<[u8]> {
        if let Some(meta_ty) = self.tys[self.file_name].get_meta_ty(expr) {
            let id = meta_ty.to_type_id(self.meta_tys, self.pointer_ty);

            return match self.module.isa().endianness() {
                Endianness::Big => Box::new(id.to_be_bytes()),
                Endianness::Little => Box::new(id.to_le_bytes()),
            };
        }

        match self.bodies_map[&module][expr].clone() {
            hir::Expr::Missing => unreachable!(),
            hir::Expr::IntLiteral(n) => {
                match (
                    self.tys[module][expr]
                        .to_comp_type(self.pointer_ty)
                        .into_number_type()
                        .unwrap()
                        .bit_width(),
                    self.module.isa().endianness(),
                ) {
                    (8, Endianness::Little) => Box::new((n as u8).to_le_bytes()),
                    (8, Endianness::Big) => Box::new((n as u8).to_be_bytes()),
                    (16, Endianness::Little) => Box::new((n as u16).to_le_bytes()),
                    (16, Endianness::Big) => Box::new((n as u16).to_be_bytes()),
                    (32, Endianness::Little) => Box::new((n as u32).to_le_bytes()),
                    (32, Endianness::Big) => Box::new((n as u32).to_be_bytes()),
                    #[allow(clippy::unnecessary_cast)]
                    (64, Endianness::Little) => Box::new((n as u64).to_le_bytes()),
                    #[allow(clippy::unnecessary_cast)]
                    (64, Endianness::Big) => Box::new((n as u64).to_be_bytes()),
                    (128, Endianness::Little) => Box::new((n as u128).to_le_bytes()),
                    (128, Endianness::Big) => Box::new((n as u128).to_be_bytes()),
                    _ => unreachable!(),
                }
            }
            hir::Expr::FloatLiteral(f) => match (
                self.tys[module][expr]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                    .unwrap()
                    .bit_width(),
                self.module.isa().endianness(),
            ) {
                (32, Endianness::Little) => Box::new((f as f32).to_le_bytes()),
                (32, Endianness::Big) => Box::new((f as f32).to_be_bytes()),
                #[allow(clippy::unnecessary_cast)]
                (64, Endianness::Little) => Box::new((f as f64).to_le_bytes()),
                #[allow(clippy::unnecessary_cast)]
                (64, Endianness::Big) => Box::new((f as f64).to_be_bytes()),
                _ => unreachable!(),
            },
            hir::Expr::BoolLiteral(b) => Box::new([b as u8]),
            hir::Expr::StringLiteral(mut text) => {
                text.push('\0');
                text.into_bytes().into()
            }
            hir::Expr::Array {
                items: Some(items), ..
            } => {
                assert_ne!(items.len(), 0);

                let item_ty = self.tys[module][items[0]];
                let item_size = item_ty.size();
                let item_stride = item_ty.stride();

                let mut array = Vec::<u8>::with_capacity(item_stride as usize * items.len());

                for (idx, item) in items.into_iter().enumerate() {
                    let item = self.expr_to_const_data(module, item);

                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            item.as_ptr(),
                            array.as_mut_ptr().add(idx * item_stride as usize),
                            item_size as usize,
                        );
                    }
                }

                unsafe { array.set_len(array.capacity()) }

                array.into()
            }
            hir::Expr::Comptime(comptime) => {
                let ctc = ComptimeToCompile {
                    file_name: self.file_name,
                    comptime,
                };

                if let Some(result) = self.comptime_results.get(&ctc) {
                    result.clone().into_bytes().unwrap()
                } else {
                    todo!("Oh shit I forgot to account for this possibility");
                }
            }
            _ => panic!(
                "tried to compile global with non-compilable definition #{}",
                expr.into_raw()
            ),
        }
    }

    fn compile_global_binding_data(&mut self, fqn: hir::Fqn) -> DataId {
        if let Some(global) = self.globals.get(&fqn) {
            return *global;
        }

        let value = self.bodies_map[&fqn.file].global_body(fqn.name);

        let bytes = self.expr_to_const_data(fqn.file, value);

        let global =
            self.create_global_data(&fqn.to_mangled_name(self.mod_dir, self.interner), bytes);

        self.globals.insert(fqn, global);

        global
    }

    fn create_global_data(&mut self, name: &str, data: Box<[u8]>) -> DataId {
        self.data_description.define(data);
        let id = self
            .module
            .declare_data(name, Linkage::Export, true, false)
            .expect("error declaring data");

        self.module
            .define_data(id, self.data_description)
            .expect("error defining data");
        self.data_description.clear();

        id
    }

    fn create_global_str(&mut self, mut text: String) -> DataId {
        text.push('\0');
        let name = format!(".str{}", self.str_id_gen.generate_unique_id());
        self.create_global_data(&name, text.into_bytes().into_boxed_slice())
    }

    fn get_func_id(&mut self, fqn: hir::Fqn) -> FuncId {
        super::get_func_id(
            self.module,
            self.pointer_ty,
            self.mod_dir,
            self.functions,
            self.compiler_defined_functions,
            self.functions_to_compile,
            self.tys,
            self.bodies_map,
            self.interner,
            fqn,
        )
    }

    fn get_local_func(&mut self, fqn: hir::Fqn) -> FuncRef {
        if let Some(func_ref) = self.local_functions.get(&fqn) {
            return *func_ref;
        }

        let func_id = self.get_func_id(fqn);

        let local_func = self.module.declare_func_in_func(func_id, self.builder.func);

        self.local_functions.insert(fqn, local_func);

        local_func
    }

    fn compile_global(&mut self, fqn: hir::Fqn, no_load: bool) -> Option<Value> {
        let ty = &self.tys[fqn].0;

        if ty.is_zero_sized() {
            return None;
        } else if ty.is_function() {
            let local_func = self.get_local_func(fqn);

            return Some(self.builder.ins().func_addr(self.pointer_ty, local_func));
        }

        let global_data = self.compile_global_binding_data(fqn);

        let local_id = self
            .module
            .declare_data_in_func(global_data, self.builder.func);

        let global_ptr = self.builder.ins().symbol_value(self.pointer_ty, local_id);

        let comp_ty = ty.to_comp_type(self.pointer_ty);

        if no_load || comp_ty.is_pointer_type() {
            Some(global_ptr)
        } else {
            Some(self.builder.ins().load(
                comp_ty.into_real_type().unwrap(),
                MemFlags::trusted(),
                global_ptr,
                0,
            ))
        }
    }

    fn compile_stmt(&mut self, stmt: &Idx<hir::Stmt>) {
        match self.bodies_map[&self.file_name][*stmt] {
            hir::Stmt::Expr(expr) => {
                match *self.tys[self.file_name][expr] {
                    hir_ty::Ty::Unknown => unreachable!(),
                    _ => {
                        self.compile_expr(expr);
                    }
                };
            }
            hir::Stmt::LocalDef(local_def) => {
                let value = self.bodies_map[&self.file_name][local_def].value;

                let ty = &self.tys[self.file_name][local_def];

                if ty.is_zero_sized() {
                    return;
                }

                let size = ty.size();

                let stack_slot = self.builder.create_sized_stack_slot(StackSlotData {
                    kind: StackSlotKind::ExplicitSlot,
                    size,
                });

                let stack_addr = self
                    .builder
                    .ins()
                    .stack_addr(self.pointer_ty, stack_slot, 0);

                self.store_expr_in_memory(value, *ty, size, stack_slot, stack_addr, 0);

                self.locals.insert(local_def, stack_addr);
            }
            hir::Stmt::Assign(assign) => {
                let assign_body = &self.bodies_map[&self.file_name][assign];

                let value_ty = &self.tys[self.file_name][assign_body.value];

                let source =
                    if let Some(val) = self.compile_expr_with_args(assign_body.source, true) {
                        val
                    } else {
                        return;
                    };

                let value = if let Some(val) = self.compile_expr(assign_body.value) {
                    val
                } else {
                    return;
                };

                if value_ty.is_aggregate() {
                    let size = value_ty.size();
                    let size = self.builder.ins().iconst(self.pointer_ty, size as i64);

                    self.builder
                        .call_memcpy(self.module.target_config(), source, value, size)
                } else {
                    self.builder
                        .ins()
                        .store(MemFlags::trusted(), value, source, 0);
                }
            }
            hir::Stmt::Break {
                label: Some(label),
                value,
                ..
            } => {
                let exit_block = self.exits[&label];

                if let Some(value) = value {
                    let value_ty = self.tys[self.file_name][value];
                    let Some(value) = self.compile_expr(value) else {
                        self.builder.ins().jump(exit_block, &[]);
                        return;
                    };

                    let value = if let Some(value_ty) =
                        value_ty.to_comp_type(self.pointer_ty).into_number_type()
                    {
                        let referenced_block_ty = self.tys[self.file_name]
                            [self.bodies_map[&self.file_name][label]]
                            .to_comp_type(self.pointer_ty)
                            .into_number_type()
                            .unwrap();

                        super::cast(&mut self.builder, value, value_ty, referenced_block_ty)
                    } else {
                        value
                    };

                    self.builder.ins().jump(exit_block, &[value]);
                } else {
                    self.builder.ins().jump(exit_block, &[]);
                };
            }
            hir::Stmt::Break { label: None, .. } => unreachable!(),
            hir::Stmt::Continue {
                label: Some(label), ..
            } => {
                let continue_block = self.exits[&label];

                self.builder.ins().jump(continue_block, &[]);
            }
            hir::Stmt::Continue { label: None, .. } => unreachable!(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn store_expr_in_memory(
        &mut self,
        expr: Idx<hir::Expr>,
        expr_ty: Intern<Ty>,
        expr_size: u32,
        stack_slot: StackSlot,
        stack_addr: Value,
        offset: u32,
    ) {
        match &self.bodies_map[&self.file_name][expr] {
            hir::Expr::Array {
                items: Some(items), ..
            } => self.store_array_items(items.clone(), stack_slot, stack_addr, offset),
            hir::Expr::StructLiteral {
                fields: field_values,
                ..
            } => self.store_struct_fields(
                expr_ty,
                field_values.iter().map(|(_, val)| *val).collect(),
                stack_slot,
                stack_addr,
                offset,
            ),
            _ if expr_ty.is_aggregate() => {
                let far_off_thing = self.compile_expr(expr).unwrap();

                let offset = self.builder.ins().iconst(self.pointer_ty, offset as i64);

                let actual_addr = self.builder.ins().iadd(stack_addr, offset);

                let size = self.builder.ins().iconst(self.pointer_ty, expr_size as i64);

                self.builder.call_memcpy(
                    self.module.target_config(),
                    actual_addr,
                    far_off_thing,
                    size,
                )
            }
            _ => {
                if let Some(item) = self.compile_expr(expr) {
                    self.builder
                        .ins()
                        .stack_store(item, stack_slot, offset as i32);
                }
            }
        }
    }

    fn store_struct_fields(
        &mut self,
        struct_ty: Intern<Ty>,
        field_values: Vec<Idx<hir::Expr>>,
        stack_slot: StackSlot,
        stack_addr: Value,
        offset: u32,
    ) {
        debug_assert!(struct_ty.is_struct());

        let field_tys = struct_ty.as_struct().unwrap();
        let struct_mem = struct_ty.struct_layout().unwrap();

        for (idx, value) in field_values.into_iter().enumerate() {
            let field_ty = field_tys[idx].1;
            let field_size = field_ty.size();

            self.store_expr_in_memory(
                value,
                field_ty,
                field_size,
                stack_slot,
                stack_addr,
                offset + struct_mem.offsets()[idx],
            );
        }
    }

    fn store_array_items(
        &mut self,
        items: Vec<Idx<hir::Expr>>,
        stack_slot: StackSlot,
        stack_addr: Value,
        offset: u32,
    ) {
        assert!(!items.is_empty());

        let inner_ty = self.tys[self.file_name][items[0]];
        let inner_stride = inner_ty.stride();

        for (idx, item) in items.into_iter().enumerate() {
            self.store_expr_in_memory(
                item,
                inner_ty,
                inner_stride,
                stack_slot,
                stack_addr,
                offset + (inner_stride * idx as u32),
            )
        }
    }

    fn compile_expr(&mut self, expr: Idx<hir::Expr>) -> Option<Value> {
        self.compile_expr_with_args(expr, false)
    }

    fn compile_expr_with_args(&mut self, expr: Idx<hir::Expr>, no_load: bool) -> Option<Value> {
        if let Some(meta_ty) = self.tys[self.file_name].get_meta_ty(expr) {
            let id = meta_ty.to_type_id(self.meta_tys, self.pointer_ty);

            return Some(self.builder.ins().iconst(types::I32, id as i64));
        }

        match self.bodies_map[&self.file_name][expr].clone() {
            hir::Expr::Missing => unreachable!(),
            hir::Expr::IntLiteral(n) => {
                let number_ty = self.tys[self.file_name][expr]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                    .unwrap();
                if number_ty.float {
                    match number_ty.bit_width() {
                        32 => Some(self.builder.ins().f32const(n as f32)),
                        64 => Some(self.builder.ins().f64const(n as f64)),
                        _ => unreachable!(),
                    }
                } else {
                    Some(match number_ty.bit_width() {
                        128 => todo!(),
                        _ => self.builder.ins().iconst(number_ty.ty, n as i64),
                    })
                }
            }
            hir::Expr::FloatLiteral(f) => {
                match self.tys[self.file_name][expr]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                    .unwrap()
                    .bit_width()
                {
                    32 => Some(self.builder.ins().f32const(f as f32)),
                    64 => Some(self.builder.ins().f64const(f)),
                    _ => unreachable!(),
                }
            }
            hir::Expr::BoolLiteral(b) => Some(self.builder.ins().iconst(types::I8, b as i64)),
            hir::Expr::StringLiteral(text) => {
                let data = self.create_global_str(text);

                let local_id = self.module.declare_data_in_func(data, self.builder.func);

                Some(self.builder.ins().symbol_value(self.pointer_ty, local_id))
            }
            hir::Expr::CharLiteral(char) => Some(self.builder.ins().iconst(types::I8, char as i64)),
            hir::Expr::Array {
                items: Some(items), ..
            } => {
                if self.tys[self.file_name][expr].is_zero_sized() {
                    return None;
                }

                let array_size = self.tys[self.file_name][expr].size();

                let stack_slot = self.builder.create_sized_stack_slot(StackSlotData {
                    kind: StackSlotKind::ExplicitSlot,
                    size: array_size,
                });

                let stack_addr = self
                    .builder
                    .ins()
                    .stack_addr(self.pointer_ty, stack_slot, 0);

                self.store_array_items(items, stack_slot, stack_addr, 0);

                Some(stack_addr)
            }
            hir::Expr::Array { items: None, .. } => None,
            hir::Expr::Index { array, index } => {
                if self.tys[self.file_name][expr].is_zero_sized() {
                    return None;
                }

                let mut array_ty = self.tys[self.file_name][array];
                let mut array = self.compile_expr(array).unwrap(); // this will be usize

                let mut required_derefs = 0;
                while let Some((_, sub_ty)) = array_ty.as_pointer() {
                    array_ty = sub_ty;
                    required_derefs += 1;
                }

                for _ in 1..required_derefs {
                    array = self
                        .builder
                        .ins()
                        .load(self.pointer_ty, MemFlags::trusted(), array, 0);
                }

                let index_ty = self.tys[self.file_name][index]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                    .unwrap();

                let index = self.compile_expr(index).unwrap();

                // make sure that the index is a usize before proceeding
                let naive_index = super::cast(
                    &mut self.builder,
                    index,
                    index_ty,
                    NumberType {
                        ty: self.pointer_ty,
                        float: false,
                        signed: false,
                    },
                );

                // now we have to align the index, the elements of the array only start every
                // so many bytes (4 bytes for i32, 8 bytes for i64)
                // So the index has to be multiplied by the element size
                let element_ty = self.tys[self.file_name][expr];

                let proper_index = self
                    .builder
                    .ins()
                    .imul_imm(naive_index, element_ty.stride() as i64);

                let proper_addr = self.builder.ins().iadd(array, proper_index);

                if no_load || element_ty.is_aggregate() {
                    Some(proper_addr)
                } else {
                    Some(
                        self.builder.ins().load(
                            element_ty
                                .to_comp_type(self.pointer_ty)
                                .into_real_type()
                                .unwrap(),
                            MemFlags::new().with_aligned(),
                            proper_addr,
                            0,
                        ),
                    )
                }
            }
            hir::Expr::Cast {
                expr: inner_expr, ..
            } => {
                let inner = self.compile_expr(inner_expr)?;
                let cast_from = match self.tys[self.file_name][inner_expr]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                {
                    Some(int_ty) => int_ty,
                    None => return Some(inner),
                };
                let cast_to = self.tys[self.file_name][expr]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                    .unwrap();

                Some(super::cast(&mut self.builder, inner, cast_from, cast_to))
            }
            hir::Expr::Ref { expr, .. } => {
                if self.tys[self.file_name][expr].is_aggregate() {
                    // references to aggregate data should return the actual address of the aggregate data
                    let expr = self.compile_expr_with_args(expr, false).unwrap();

                    Some(expr)
                } else if matches!(
                    self.bodies_map[&self.file_name][expr],
                    hir::Expr::Local(_) | hir::Expr::LocalGlobal(_)
                ) {
                    // references to locals or globals should return the actual memory address of the local or global
                    self.compile_expr_with_args(expr, true)
                } else {
                    let inner_size = self.tys[self.file_name][expr].size();

                    // println!("{:?} = {inner_size}", self.tys[self.fqn.module][expr]);

                    let stack_slot = self.builder.create_sized_stack_slot(StackSlotData {
                        kind: StackSlotKind::ExplicitSlot,
                        size: inner_size,
                    });

                    let expr = self.compile_expr(expr).unwrap();

                    self.builder.ins().stack_store(expr, stack_slot, 0);

                    Some(
                        self.builder
                            .ins()
                            .stack_addr(self.pointer_ty, stack_slot, 0),
                    )
                }
            }
            hir::Expr::Deref { pointer } => {
                let self_ty = self.tys[self.file_name][expr];

                if self_ty.is_aggregate() {
                    return self.compile_expr_with_args(pointer, no_load);
                }

                let addr = self.compile_expr_with_args(pointer, no_load)?;

                let self_ty = self_ty.to_comp_type(self.pointer_ty);

                let self_ty = if no_load {
                    self.pointer_ty
                } else {
                    self_ty.into_real_type().unwrap()
                };

                Some(
                    self.builder
                        .ins()
                        .load(self_ty, MemFlags::trusted(), addr, 0),
                )
            }
            hir::Expr::Binary {
                lhs: lhs_expr,
                rhs: rhs_expr,
                op,
            } => {
                match op {
                    hir::BinaryOp::LAnd => {
                        let rhs_block = self.builder.create_block();
                        let exit_block = self.builder.create_block();

                        // if lhs is true, test the rhs
                        // if lhs is false, exit early
                        let lhs = self.compile_expr(lhs_expr).unwrap();
                        self.builder
                            .ins()
                            .brif(lhs, rhs_block, &[], exit_block, &[lhs]);

                        self.builder.switch_to_block(rhs_block);
                        self.builder.seal_block(rhs_block);

                        let rhs = self.compile_expr(rhs_expr).unwrap();
                        self.builder.ins().jump(exit_block, &[rhs]);

                        self.builder.switch_to_block(exit_block);
                        self.builder.seal_block(exit_block);
                        let result = self.builder.append_block_param(exit_block, types::I8);

                        return Some(result);
                    }
                    hir::BinaryOp::LOr => {
                        let rhs_block = self.builder.create_block();
                        let exit_block = self.builder.create_block();

                        // if the lhs is true, exit early
                        // if the lhs is false, test the rhs
                        let lhs = self.compile_expr(lhs_expr).unwrap();
                        self.builder
                            .ins()
                            .brif(lhs, exit_block, &[lhs], rhs_block, &[]);

                        self.builder.switch_to_block(rhs_block);
                        self.builder.seal_block(rhs_block);

                        let rhs = self.compile_expr(rhs_expr).unwrap();
                        self.builder.ins().jump(exit_block, &[rhs]);

                        self.builder.switch_to_block(exit_block);
                        self.builder.seal_block(exit_block);
                        let result = self.builder.append_block_param(exit_block, types::I8);

                        return Some(result);
                    }
                    _ => {}
                }

                let lhs = self.compile_expr(lhs_expr).unwrap();
                let rhs = self.compile_expr(rhs_expr).unwrap();

                let lhs_ty = self.tys[self.file_name][lhs_expr]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                    .unwrap();
                let rhs_ty = self.tys[self.file_name][rhs_expr]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                    .unwrap();

                let max_ty = lhs_ty.max(rhs_ty);

                // we need to make sure that both types are the same before we can do any operations on them
                let lhs = super::cast(&mut self.builder, lhs, lhs_ty, max_ty);
                let rhs = super::cast(&mut self.builder, rhs, rhs_ty, max_ty);

                if max_ty.float {
                    Some(match op {
                        hir::BinaryOp::Add => self.builder.ins().fadd(lhs, rhs),
                        hir::BinaryOp::Sub => self.builder.ins().fsub(lhs, rhs),
                        hir::BinaryOp::Mul => self.builder.ins().fmul(lhs, rhs),
                        hir::BinaryOp::Div => self.builder.ins().fdiv(lhs, rhs),
                        hir::BinaryOp::Mod => unreachable!(),
                        hir::BinaryOp::Lt => self.builder.ins().fcmp(FloatCC::LessThan, lhs, rhs),
                        hir::BinaryOp::Gt => {
                            self.builder.ins().fcmp(FloatCC::GreaterThan, lhs, rhs)
                        }
                        hir::BinaryOp::Le => {
                            self.builder.ins().fcmp(FloatCC::LessThanOrEqual, lhs, rhs)
                        }
                        hir::BinaryOp::Ge => {
                            self.builder
                                .ins()
                                .fcmp(FloatCC::GreaterThanOrEqual, lhs, rhs)
                        }
                        hir::BinaryOp::Eq => self.builder.ins().fcmp(FloatCC::Equal, lhs, rhs),
                        hir::BinaryOp::Ne => self.builder.ins().fcmp(FloatCC::NotEqual, lhs, rhs),
                        hir::BinaryOp::BAnd => self.builder.ins().band(lhs, rhs),
                        hir::BinaryOp::BOr => self.builder.ins().bor(lhs, rhs),
                        hir::BinaryOp::Xor => self.builder.ins().bxor(lhs, rhs),
                        hir::BinaryOp::LShift | hir::BinaryOp::RShift => unreachable!(),
                        hir::BinaryOp::LAnd | hir::BinaryOp::LOr => unreachable!(),
                    })
                } else {
                    Some(match op {
                        hir::BinaryOp::Add => self.builder.ins().iadd(lhs, rhs),
                        hir::BinaryOp::Sub => self.builder.ins().isub(lhs, rhs),
                        hir::BinaryOp::Mul => self.builder.ins().imul(lhs, rhs),
                        hir::BinaryOp::Div => {
                            if max_ty.signed {
                                self.builder.ins().sdiv(lhs, rhs)
                            } else {
                                self.builder.ins().udiv(lhs, rhs)
                            }
                        }
                        hir::BinaryOp::Mod => {
                            if max_ty.signed {
                                self.builder.ins().srem(lhs, rhs)
                            } else {
                                self.builder.ins().urem(lhs, rhs)
                            }
                        }
                        hir::BinaryOp::Lt => {
                            if max_ty.signed {
                                self.builder.ins().icmp(IntCC::SignedLessThan, lhs, rhs)
                            } else {
                                self.builder.ins().icmp(IntCC::UnsignedLessThan, lhs, rhs)
                            }
                        }
                        hir::BinaryOp::Gt => {
                            if max_ty.signed {
                                self.builder.ins().icmp(IntCC::SignedGreaterThan, lhs, rhs)
                            } else {
                                self.builder
                                    .ins()
                                    .icmp(IntCC::UnsignedGreaterThan, lhs, rhs)
                            }
                        }
                        hir::BinaryOp::Le => {
                            if max_ty.signed {
                                self.builder
                                    .ins()
                                    .icmp(IntCC::SignedLessThanOrEqual, lhs, rhs)
                            } else {
                                self.builder
                                    .ins()
                                    .icmp(IntCC::UnsignedLessThanOrEqual, lhs, rhs)
                            }
                        }
                        hir::BinaryOp::Ge => {
                            if max_ty.signed {
                                self.builder
                                    .ins()
                                    .icmp(IntCC::SignedGreaterThanOrEqual, lhs, rhs)
                            } else {
                                self.builder
                                    .ins()
                                    .icmp(IntCC::UnsignedGreaterThanOrEqual, lhs, rhs)
                            }
                        }
                        hir::BinaryOp::Eq => self.builder.ins().icmp(IntCC::Equal, lhs, rhs),
                        hir::BinaryOp::Ne => self.builder.ins().icmp(IntCC::NotEqual, lhs, rhs),
                        hir::BinaryOp::BAnd => self.builder.ins().band(lhs, rhs),
                        hir::BinaryOp::BOr => self.builder.ins().bor(lhs, rhs),
                        hir::BinaryOp::Xor => self.builder.ins().bxor(lhs, rhs),
                        hir::BinaryOp::LShift => self.builder.ins().ishl(lhs, rhs),
                        hir::BinaryOp::RShift => {
                            if max_ty.signed {
                                self.builder.ins().sshr(lhs, rhs)
                            } else {
                                self.builder.ins().ushr(lhs, rhs)
                            }
                        }
                        hir::BinaryOp::LAnd | hir::BinaryOp::LOr => unreachable!(),
                    })
                }
            }
            hir::Expr::Unary { expr, op } => {
                let expr_ty = self.tys[self.file_name][expr]
                    .to_comp_type(self.pointer_ty)
                    .into_number_type()
                    .unwrap();

                let expr = self.compile_expr(expr).unwrap();

                if expr_ty.float {
                    match op {
                        hir::UnaryOp::Pos => Some(expr),
                        hir::UnaryOp::Neg => Some(self.builder.ins().fneg(expr)),
                        hir::UnaryOp::BNot => Some(self.builder.ins().bnot(expr)),
                        hir::UnaryOp::LNot => unreachable!(),
                    }
                } else {
                    match op {
                        hir::UnaryOp::Pos => Some(expr),
                        hir::UnaryOp::Neg => Some(self.builder.ins().ineg(expr)),
                        hir::UnaryOp::BNot => Some(self.builder.ins().bnot(expr)),
                        hir::UnaryOp::LNot => {
                            let zero = self.builder.ins().iconst(expr_ty.ty, 0);
                            Some(self.builder.ins().icmp(IntCC::Equal, expr, zero))
                        }
                    }
                }
            }
            hir::Expr::Call { callee, args } => {
                let (param_tys, return_ty) = self.tys[self.file_name][callee]
                    .clone()
                    .as_function()
                    .unwrap();

                let mut arg_values = args
                    .iter()
                    .zip(param_tys.iter())
                    .filter_map(|(arg_expr, expected_ty)| {
                        let arg_ty = self.tys[self.file_name][*arg_expr];
                        let comp_ty = arg_ty.to_comp_type(self.pointer_ty);

                        let arg = self.compile_expr(*arg_expr);

                        if let Some(actual_ty) = comp_ty.into_number_type() {
                            let expected_ty = expected_ty
                                .to_comp_type(self.pointer_ty)
                                .into_number_type()
                                .unwrap();

                            Some(super::cast(
                                &mut self.builder,
                                arg.unwrap(),
                                actual_ty,
                                expected_ty,
                            ))
                        } else {
                            arg
                        }
                    })
                    .collect::<Vec<_>>();

                if return_ty.is_aggregate() {
                    let aggregate_size = return_ty.size();

                    let stack_slot = self.builder.create_sized_stack_slot(StackSlotData {
                        kind: StackSlotKind::ExplicitSlot,
                        size: aggregate_size,
                    });
                    let stack_slot_addr =
                        self.builder
                            .ins()
                            .stack_addr(self.pointer_ty, stack_slot, 0);

                    arg_values.push(stack_slot_addr);
                }

                let call = match self.bodies_map[&self.file_name][callee] {
                    hir::Expr::LocalGlobal(name) => {
                        let fqn = hir::Fqn {
                            file: self.file_name,
                            name: name.name,
                        };

                        let local_func = self.get_local_func(fqn);

                        self.builder.ins().call(local_func, &arg_values)
                    }
                    hir::Expr::Local(local) if !self.bodies_map[&self.file_name][local].mutable => {
                        let value = self.bodies_map[&self.file_name][local].value;

                        if let hir::Expr::Lambda(lambda) = self.bodies_map[&self.file_name][value] {
                            let local_func = self.unnamed_func_to_local(callee, lambda);

                            self.builder.ins().call(local_func, &arg_values)
                        } else {
                            let callee = self.compile_expr(callee).unwrap();

                            let (comp_sig, _) = (&param_tys, return_ty)
                                .to_cranelift_signature(self.module, self.pointer_ty);

                            let sig_ref = self.builder.import_signature(comp_sig);

                            self.builder
                                .ins()
                                .call_indirect(sig_ref, callee, &arg_values)
                        }
                    }
                    hir::Expr::Path {
                        previous, field, ..
                    } => match &self.tys[self.file_name][previous].as_ref() {
                        Ty::File(file) => {
                            let fqn = hir::Fqn {
                                file: *file,
                                name: field.name,
                            };

                            let local_func = self.get_local_func(fqn);

                            self.builder.ins().call(local_func, &arg_values)
                        }
                        _ => {
                            let callee = self.compile_expr(callee).unwrap();

                            let (comp_sig, _) = (&param_tys, return_ty)
                                .to_cranelift_signature(self.module, self.pointer_ty);

                            let sig_ref = self.builder.import_signature(comp_sig);

                            self.builder
                                .ins()
                                .call_indirect(sig_ref, callee, &arg_values)
                        }
                    },
                    hir::Expr::Lambda(lambda) => {
                        let local_func = self.unnamed_func_to_local(callee, lambda);

                        self.builder.ins().call(local_func, &arg_values)
                    }
                    _ => {
                        let callee = self.compile_expr(callee).unwrap();

                        let (comp_sig, _) = (&param_tys, return_ty)
                            .to_cranelift_signature(self.module, self.pointer_ty);

                        let sig_ref = self.builder.import_signature(comp_sig);

                        self.builder
                            .ins()
                            .call_indirect(sig_ref, callee, &arg_values)
                    }
                };

                if return_ty.is_zero_sized() {
                    None
                } else {
                    Some(self.builder.inst_results(call)[0])
                }
            }
            hir::Expr::Block { stmts, tail_expr } => {
                let ty = self.tys[self.file_name][expr].to_comp_type(self.pointer_ty);

                let body_block = self.builder.create_block();
                let exit_block = self.builder.create_block();
                if let Some(ty) = ty.into_real_type() {
                    self.builder.append_block_param(exit_block, ty);
                }
                if let Some(scope_id) = self.bodies_map[&self.file_name].block_to_scope_id(expr) {
                    self.exits.insert(scope_id, exit_block);
                }

                self.builder.ins().jump(body_block, &[]);

                self.builder.switch_to_block(body_block);
                self.builder.seal_block(body_block);

                let mut did_break = false;
                for stmt in stmts {
                    self.compile_stmt(&stmt);
                    if matches!(
                        self.bodies_map[&self.file_name][stmt],
                        hir::Stmt::Break { .. } | hir::Stmt::Continue { .. }
                    ) {
                        did_break = true;
                        break;
                    }
                }

                if let Some(value) = tail_expr {
                    if !did_break {
                        if let Some(value) = self.compile_expr_with_args(value, no_load) {
                            self.builder.ins().jump(exit_block, &[value]);
                        } else {
                            self.builder.ins().jump(exit_block, &[]);
                        };
                    }

                    self.builder.switch_to_block(exit_block);
                    self.builder.seal_block(exit_block);

                    if ty.into_real_type().is_some() {
                        Some(self.builder.block_params(exit_block)[0])
                    } else {
                        None
                    }
                } else {
                    if !did_break {
                        self.builder.ins().jump(exit_block, &[]);
                    }

                    self.builder.switch_to_block(exit_block);
                    self.builder.seal_block(exit_block);

                    None
                }
            }
            hir::Expr::If {
                condition,
                body,
                else_branch,
            } => {
                let condition = self.compile_expr(condition).unwrap();

                // build branch
                let then_block = self.builder.create_block();
                let else_block = self.builder.create_block();
                let merge_block = self.builder.create_block();

                let return_ty = self.tys[self.file_name][expr]
                    .to_comp_type(self.pointer_ty)
                    .into_real_type();

                if let Some(return_ty) = return_ty {
                    self.builder.append_block_param(merge_block, return_ty);
                }

                self.builder
                    .ins()
                    .brif(condition, then_block, &[], else_block, &[]);

                // build then block

                self.builder.switch_to_block(then_block);
                self.builder.seal_block(then_block);

                match self.compile_expr(body) {
                    Some(then_value) => {
                        self.builder.ins().jump(merge_block, &[then_value]);
                    }
                    None => {
                        self.builder.ins().jump(merge_block, &[]);
                    }
                }

                // build else block

                self.builder.switch_to_block(else_block);
                self.builder.seal_block(else_block);

                match else_branch.and_then(|else_branch| self.compile_expr(else_branch)) {
                    Some(then_value) => {
                        self.builder.ins().jump(merge_block, &[then_value]);
                    }
                    None => {
                        self.builder.ins().jump(merge_block, &[]);
                    }
                }

                // build merge block

                self.builder.switch_to_block(merge_block);
                self.builder.seal_block(merge_block);

                if return_ty.is_some() {
                    let phi = self.builder.block_params(merge_block)[0];

                    Some(phi)
                } else {
                    None
                }
            }
            hir::Expr::While { condition, body } => {
                let header_block = self.builder.create_block();
                let body_block = self.builder.create_block();
                let exit_block = self.builder.create_block();

                let ty = self.tys[self.file_name][expr].to_comp_type(self.pointer_ty);

                if let Some(ty) = ty.into_real_type() {
                    self.builder.append_block_param(exit_block, ty);
                }
                if let Some(scope_id) = self.bodies_map[&self.file_name].block_to_scope_id(expr) {
                    self.continues.insert(scope_id, header_block);
                    self.exits.insert(scope_id, exit_block);
                }

                self.builder.ins().jump(header_block, &[]);
                self.builder.switch_to_block(header_block);
                // don't seal the header yet

                if let Some(condition) =
                    condition.and_then(|condition| self.compile_expr(condition))
                {
                    self.builder
                        .ins()
                        .brif(condition, body_block, &[], exit_block, &[]);
                } else {
                    self.builder.ins().jump(body_block, &[]);
                }

                self.builder.switch_to_block(body_block);
                self.builder.seal_block(body_block);

                self.compile_expr(body);

                self.builder.ins().jump(header_block, &[]);

                // We've reached the bottom of the loop, so there will be no
                // more jumps to the header
                self.builder.seal_block(header_block);

                self.builder.switch_to_block(exit_block);
                self.builder.seal_block(exit_block);

                if ty.into_real_type().is_some() {
                    Some(self.builder.block_params(exit_block)[0])
                } else {
                    None
                }
            }
            hir::Expr::Local(local_def) => {
                let ptr = *self.locals.get(&local_def)?;

                let ty = &self.tys[self.file_name][local_def];

                if no_load || ty.is_aggregate() {
                    Some(ptr)
                } else {
                    let ty = ty.to_comp_type(self.pointer_ty);

                    Some(self.builder.ins().load(
                        ty.into_real_type().unwrap(),
                        MemFlags::trusted(),
                        ptr,
                        0,
                    ))
                }
            }
            hir::Expr::Param { idx, .. } => self
                .params
                .get(&(idx as u64))
                .map(|param| self.builder.use_var(*param)),
            hir::Expr::LocalGlobal(name) => {
                if self.tys[self.file_name][expr].is_zero_sized() {
                    return None;
                }

                let fqn = hir::Fqn {
                    file: self.file_name,
                    name: name.name,
                };

                self.compile_global(fqn, no_load)
            }
            hir::Expr::Path {
                previous, field, ..
            } => {
                if self.tys[self.file_name][expr].is_zero_sized() {
                    return None;
                }

                let previous_ty = self.tys[self.file_name][previous];
                match previous_ty.as_ref() {
                    Ty::File(file) => {
                        let fqn = hir::Fqn {
                            file: *file,
                            name: field.name,
                        };

                        self.compile_global(fqn, no_load)
                    }
                    _ => {
                        let field_ty = &self.tys[self.file_name][expr];
                        let field_comp_ty =
                            field_ty.to_comp_type(self.pointer_ty).into_real_type()?;

                        let mut required_derefs = 0;
                        let mut struct_ty = previous_ty;
                        while let Some((_, sub_ty)) = struct_ty.as_pointer() {
                            struct_ty = sub_ty;
                            required_derefs += 1;
                        }

                        let struct_fields = struct_ty.as_struct().unwrap();

                        let field_idx = struct_fields
                            .iter()
                            .enumerate()
                            .find(|(_, (name, _))| *name == field.name)
                            .map(|(idx, _)| idx)
                            .unwrap();

                        let offset = struct_ty.struct_layout().unwrap().offsets()[field_idx];

                        let mut struct_addr = self.compile_expr_with_args(previous, false)?;

                        for _ in 1..required_derefs {
                            struct_addr = self.builder.ins().load(
                                self.pointer_ty,
                                MemFlags::trusted(),
                                struct_addr,
                                0,
                            );
                        }

                        if no_load || field_ty.is_aggregate() {
                            Some(self.builder.ins().iadd_imm(struct_addr, offset as i64))
                        } else {
                            Some(self.builder.ins().load(
                                field_comp_ty,
                                MemFlags::trusted(),
                                struct_addr,
                                offset as i32,
                            ))
                        }
                    }
                }
            }
            hir::Expr::Lambda(lambda) => {
                let local_func = self.unnamed_func_to_local(expr, lambda);

                Some(self.builder.ins().func_addr(self.pointer_ty, local_func))
            }
            hir::Expr::StructLiteral {
                fields: field_values,
                ..
            } => {
                let ty = self.tys[self.file_name][expr];

                let stack_slot = self.builder.create_sized_stack_slot(StackSlotData {
                    kind: StackSlotKind::ExplicitSlot,
                    size: ty.size(),
                });

                let stack_addr = self
                    .builder
                    .ins()
                    .stack_addr(self.pointer_ty, stack_slot, 0);

                self.store_struct_fields(
                    ty,
                    field_values.iter().map(|(_, val)| *val).collect(),
                    stack_slot,
                    stack_addr,
                    0,
                );

                Some(stack_addr)
            }
            hir::Expr::PrimitiveTy { .. } => None,
            hir::Expr::Distinct { .. } => None,
            hir::Expr::StructDecl { .. } => None,
            hir::Expr::Import(_) => None,
            hir::Expr::Comptime(comptime) => {
                let ctc = ComptimeToCompile {
                    file_name: self.file_name,
                    comptime,
                };

                // if the comptime block was evaluated in a previous compilation step, then get that value
                // otherwise, we are *in* the comptime eval step of compilation, and so just calculate it's value
                if let Some(result) = self.comptime_results.get(&ctc) {
                    let ty = self.tys[self.file_name][expr].to_comp_type(self.pointer_ty);

                    match result {
                        ComptimeResult::Integer { num, .. } => Some(
                            self.builder
                                .ins()
                                .iconst(ty.into_real_type().unwrap(), *num as i64),
                        ),
                        ComptimeResult::Float { num, .. } => {
                            match ty.into_number_type().unwrap().bit_width() {
                                32 => Some(self.builder.ins().f32const(*num as f32)),
                                64 => Some(self.builder.ins().f64const(*num)),
                                _ => unreachable!(),
                            }
                        }
                        ComptimeResult::Data(bytes) => {
                            let data = self.create_global_data(
                                &ctc.to_mangled_name(self.mod_dir, self.interner),
                                bytes.clone(),
                            );

                            let local_id =
                                self.module.declare_data_in_func(data, self.builder.func);

                            let global_ptr =
                                self.builder.ins().symbol_value(self.pointer_ty, local_id);

                            if no_load || ty.is_pointer_type() {
                                Some(global_ptr)
                            } else {
                                Some(self.builder.ins().load(
                                    ty.into_real_type().unwrap(),
                                    MemFlags::trusted(),
                                    global_ptr,
                                    0,
                                ))
                            }
                        }
                        ComptimeResult::Void => None,
                    }
                } else {
                    self.compile_expr(self.bodies_map[&self.file_name][comptime].body)
                }
            }
        }
    }

    fn unnamed_func_to_local(&mut self, expr: Idx<hir::Expr>, lambda: Idx<hir::Lambda>) -> FuncRef {
        if let Some(func_ref) = self.local_lambdas.get(&lambda) {
            return *func_ref;
        }

        let (param_tys, return_ty) = self.tys[self.file_name][expr].as_function().unwrap();

        let (sig, _) = (&param_tys, return_ty).to_cranelift_signature(self.module, self.pointer_ty);

        let ftc = FunctionToCompile {
            file_name: self.file_name,
            function_name: None,
            lambda,
            param_tys,
            return_ty,
        };

        let mangled = ftc.to_mangled_name(self.mod_dir, self.interner);

        self.functions_to_compile.push_back(ftc);

        let func_id = self
            .module
            .declare_function(&mangled, Linkage::Export, &sig)
            .unwrap();

        let local_func = self.module.declare_func_in_func(func_id, self.builder.func);

        self.local_lambdas.insert(lambda, local_func);

        local_func
    }
}
