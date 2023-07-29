use std::vec;

use ast::{AstNode, AstToken};
use interner::{Interner, Key};
use la_arena::{Arena, ArenaMap, Idx};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::SyntaxTree;
use text_size::{TextRange, TextSize};

use crate::{
    nameres::{Path, PathWithRange},
    world_index::{GetDefinitionError, WorldIndex},
    Definition, Fqn, Function, Name, TyParseError, TyWithRange, UIDGenerator,
};

#[derive(Clone)]
pub struct Bodies {
    local_defs: Arena<LocalDef>,
    local_sets: Arena<LocalSet>,
    stmts: Arena<Stmt>,
    exprs: Arena<Expr>,
    expr_ranges: ArenaMap<Idx<Expr>, TextRange>,
    function_bodies: FxHashMap<Name, Idx<Expr>>,
    globals: FxHashMap<Name, Idx<Expr>>,
    other_module_references: FxHashSet<Fqn>,
    symbol_map: FxHashMap<ast::Ident, Symbol>,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Missing,
    IntLiteral(u64),
    BoolLiteral(bool),
    StringLiteral(String),
    Cast {
        expr: Idx<Expr>,
        ty: TyWithRange,
    },
    Ref {
        expr: Idx<Expr>,
    },
    Deref {
        pointer: Idx<Expr>,
    },
    Binary {
        lhs: Idx<Expr>,
        rhs: Idx<Expr>,
        op: BinaryOp,
    },
    Unary {
        expr: Idx<Expr>,
        op: UnaryOp,
    },
    Array {
        items: Vec<Idx<Expr>>,
        ty: TyWithRange,
    },
    Index {
        array: Idx<Expr>,
        index: Idx<Expr>,
    },
    Block {
        stmts: Vec<Idx<Stmt>>,
        tail_expr: Option<Idx<Expr>>,
    },
    If {
        condition: Idx<Expr>,
        body: Idx<Expr>,
        else_branch: Option<Idx<Expr>>,
    },
    While {
        condition: Option<Idx<Expr>>,
        body: Idx<Expr>,
    },
    Local(Idx<LocalDef>),
    Global(PathWithRange),
    Param {
        idx: u32,
    },
    Call {
        path: PathWithRange,
        args: Vec<Idx<Expr>>,
    },
    Ty {
        ty: Idx<TyWithRange>,
    },
    Distinct {
        uid: u32,
        ty: Idx<TyWithRange>,
    },
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Idx<Expr>),
    LocalDef(Idx<LocalDef>),
    LocalSet(Idx<LocalSet>),
}

#[derive(Clone)]
pub struct LocalDef {
    pub mutable: bool,
    pub ty: TyWithRange,
    pub value: Idx<Expr>,
    pub ast: ast::Define,
}

#[derive(Clone)]
pub struct LocalSet {
    pub local_def: Option<Idx<LocalDef>>,
    pub value: Idx<Expr>,
    pub ast: ast::Assign,
}

impl std::fmt::Debug for LocalDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalDef")
            .field("value", &self.value)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryOp {
    // math operations
    Add,
    Sub,
    Mul,
    Div,

    // cmp operations
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,

    // boolean operations
    And,
    Or,
}

#[derive(Debug, Clone, Copy)]
pub enum UnaryOp {
    // math operations
    Pos,
    Neg,

    // boolean operations
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoweringDiagnostic {
    pub kind: LoweringDiagnosticKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoweringDiagnosticKind {
    OutOfRangeIntLiteral,
    UndefinedLocal { name: Key },
    UndefinedModule { name: Key },
    MutableGlobal,
    SetImmutable { name: Key },
    MismatchedArgCount { name: Key, expected: u32, got: u32 },
    CalledNonLambda { name: Key },
    ArrayMissingBody,
    TyParseError(TyParseError),
    InvalidEscape,
}

#[derive(Clone, Copy)]
pub enum Symbol {
    Local(Idx<LocalDef>),
    Param(ast::Param),
    Global(Path),
    PrimitiveTy(Idx<TyWithRange>),
    Function(Path),
    Module(Name),
    Unknown,
}

pub fn lower(
    root: ast::Root,
    tree: &SyntaxTree,
    module: Name,
    world_index: &WorldIndex,
    uid_gen: &mut UIDGenerator,
    twr_arena: &mut Arena<TyWithRange>,
    interner: &mut Interner,
) -> (Bodies, Vec<LoweringDiagnostic>) {
    let mut ctx = Ctx::new(module, world_index, uid_gen, twr_arena, interner, tree);

    for def in root.defs(tree) {
        let (name, value) = match def {
            ast::Define::Binding(binding) => (binding.name(tree), binding.value(tree)),
            ast::Define::Variable(variable) => (variable.name(tree), variable.value(tree)),
        };
        match value {
            Some(ast::Expr::Lambda(lambda)) => ctx.lower_lambda(name, lambda),
            val => ctx.lower_global(name, val),
        }
    }

    ctx.bodies.shrink_to_fit();

    (ctx.bodies, ctx.diagnostics)
}

struct Ctx<'a> {
    bodies: Bodies,
    module: Name,
    world_index: &'a WorldIndex,
    uid_gen: &'a mut UIDGenerator,
    twr_arena: &'a mut Arena<TyWithRange>,
    interner: &'a mut Interner,
    tree: &'a SyntaxTree,
    diagnostics: Vec<LoweringDiagnostic>,
    scopes: Vec<FxHashMap<Key, Idx<LocalDef>>>,
    params: FxHashMap<Key, (u32, ast::Param)>,
}

impl<'a> Ctx<'a> {
    fn new(
        module: Name,
        world_index: &'a WorldIndex,
        uid_gen: &'a mut UIDGenerator,
        twr_arena: &'a mut Arena<TyWithRange>,
        interner: &'a mut Interner,
        tree: &'a SyntaxTree,
    ) -> Self {
        Self {
            bodies: Bodies {
                local_defs: Arena::new(),
                local_sets: Arena::new(),
                stmts: Arena::new(),
                exprs: Arena::new(),
                expr_ranges: ArenaMap::default(),
                function_bodies: FxHashMap::default(),
                globals: FxHashMap::default(),
                other_module_references: FxHashSet::default(),
                symbol_map: FxHashMap::default(),
            },
            module,
            world_index,
            uid_gen,
            twr_arena,
            interner,
            tree,
            diagnostics: Vec::new(),
            scopes: vec![FxHashMap::default()],
            params: FxHashMap::default(),
        }
    }

    fn lower_ty(&mut self, ty: Option<ast::Ty>) -> TyWithRange {
        match TyWithRange::parse(
            ty.and_then(|ty| ty.expr(self.tree)),
            self.uid_gen,
            self.twr_arena,
            self.interner,
            self.tree,
        ) {
            Ok(ty) => ty,
            Err(why) => {
                let range = ty.unwrap().range(self.tree);
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::TyParseError(why),
                    range,
                });

                TyWithRange::Unknown
            }
        }
    }

    fn lower_global(&mut self, name_token: Option<ast::Ident>, expr: Option<ast::Expr>) {
        let name = match name_token {
            Some(ident) => Name(self.interner.intern(ident.text(self.tree))),
            None => return,
        };

        // if we’ve already seen a global with this name,
        // we ignore all other globals with that name
        //
        // we don’t have to worry about emitting a diagnostic here
        // because indexing already handles this
        if self.bodies.globals.contains_key(&name) {
            return;
        }

        let body = self.lower_expr(expr);
        self.bodies.globals.insert(name, body);
    }

    fn lower_lambda(&mut self, name_token: Option<ast::Ident>, lambda: ast::Lambda) {
        let name = match name_token {
            Some(ident) => Name(self.interner.intern(ident.text(self.tree))),
            None => return,
        };

        // if we’ve already seen a function with this name,
        // we ignore all other functions with that name
        //
        // we don’t have to worry about emitting a diagnostic here
        // because indexing already handles this
        if self.bodies.function_bodies.contains_key(&name) {
            return;
        }

        if let Some(param_list) = lambda.param_list(self.tree) {
            for (idx, param) in param_list.params(self.tree).enumerate() {
                if let Some(ident) = param.name(self.tree) {
                    self.params.insert(
                        self.interner.intern(ident.text(self.tree)),
                        (idx as u32, param),
                    );
                }
            }
        }

        let body = self.lower_expr(lambda.body(self.tree));
        self.params.clear();
        self.bodies.function_bodies.insert(name, body);
    }

    fn lower_stmt(&mut self, stmt: ast::Stmt) -> Stmt {
        match stmt {
            ast::Stmt::Define(local_def) => self.lower_local_define(local_def),
            ast::Stmt::Assign(local_set) => self.lower_assignment(local_set),
            ast::Stmt::Expr(expr_stmt) => {
                let expr = self.lower_expr(expr_stmt.expr(self.tree));
                Stmt::Expr(expr)
            }
        }
    }

    fn lower_local_define(&mut self, local_def: ast::Define) -> Stmt {
        let ty = self.lower_ty(local_def.ty(self.tree));
        let value = self.lower_expr(local_def.value(self.tree));
        let id = self.bodies.local_defs.alloc(LocalDef {
            mutable: matches!(local_def, ast::Define::Variable(_)),
            ty,
            value,
            ast: local_def,
        });

        if let Some(ident) = local_def.name(self.tree) {
            let name = self.interner.intern(ident.text(self.tree));
            self.insert_into_current_scope(name, id);
        }

        Stmt::LocalDef(id)
    }

    fn lower_assignment(&mut self, local_set: ast::Assign) -> Stmt {
        let name = self
            .interner
            .intern(local_set.name(self.tree).unwrap().text(self.tree));

        let local_def = self.look_up_in_current_scope(name);
        if local_def.is_none() {
            if self
                .world_index
                .get_definition(Fqn {
                    module: self.module,
                    name: Name(name),
                })
                .is_ok()
                || self.params.contains_key(&name)
            {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::SetImmutable { name },
                    range: local_set.range(self.tree),
                })
            } else {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::UndefinedLocal { name },
                    range: local_set.name(self.tree).unwrap().range(self.tree),
                })
            }
        } else if !self.bodies.local_defs[local_def.unwrap()].mutable {
            self.diagnostics.push(LoweringDiagnostic {
                kind: LoweringDiagnosticKind::SetImmutable { name },
                range: local_set.range(self.tree),
            })
        }

        let value = self.lower_expr(local_set.value(self.tree));

        let id = self.bodies.local_sets.alloc(LocalSet {
            local_def,
            value,
            ast: local_set,
        });

        Stmt::LocalSet(id)
    }

    fn lower_expr(&mut self, expr: Option<ast::Expr>) -> Idx<Expr> {
        let expr_ast = match expr {
            Some(expr) => expr,
            None => return self.bodies.exprs.alloc(Expr::Missing),
        };

        let range = expr_ast.range(self.tree);

        let expr = self.lower_expr_raw(expr_ast);

        let id = self.bodies.exprs.alloc(expr);
        self.bodies.expr_ranges.insert(id, range);

        id
    }

    fn lower_expr_raw(&mut self, expr: ast::Expr) -> Expr {
        match expr {
            ast::Expr::Cast(cast_expr) => self.lower_cast_expr(cast_expr),
            ast::Expr::Ref(ref_expr) => self.lower_ref_expr(ref_expr),
            ast::Expr::Deref(deref_expr) => self.lower_deref_expr(deref_expr),
            ast::Expr::Binary(binary_expr) => self.lower_binary_expr(binary_expr),
            ast::Expr::Unary(unary_expr) => self.lower_unary_expr(unary_expr),
            ast::Expr::Array(array_expr) => self.lower_array_expr(array_expr),
            ast::Expr::Block(block) => self.lower_block(block),
            ast::Expr::If(if_expr) => self.lower_if(if_expr),
            ast::Expr::While(while_expr) => self.lower_while(while_expr),
            ast::Expr::Call(call) => self.lower_call(call),
            ast::Expr::IndexExpr(index_expr) => self.lower_index_expr(index_expr),
            ast::Expr::VarRef(var_ref) => self.lower_var_ref(var_ref),
            ast::Expr::IntLiteral(int_literal) => self.lower_int_literal(int_literal),
            ast::Expr::BoolLiteral(bool_literal) => self.lower_bool_literal(bool_literal),
            ast::Expr::StringLiteral(string_literal) => self.lower_string_literal(string_literal),
            ast::Expr::Distinct(distinct) => self.lower_distinct(distinct),
            ast::Expr::Lambda(_) => unreachable!(),
        }
    }

    fn lower_cast_expr(&mut self, cast_expr: ast::CastExpr) -> Expr {
        let ty = self.lower_ty(cast_expr.ty(self.tree));

        let expr = self.lower_expr(cast_expr.expr(self.tree));

        Expr::Cast { expr, ty }
    }

    fn lower_ref_expr(&mut self, ref_expr: ast::RefExpr) -> Expr {
        let expr = self.lower_expr(ref_expr.expr(self.tree));

        Expr::Ref { expr }
    }

    fn lower_deref_expr(&mut self, deref_expr: ast::DerefExpr) -> Expr {
        let pointer = self.lower_expr(deref_expr.pointer(self.tree));

        Expr::Deref { pointer }
    }

    fn lower_distinct(&mut self, distinct: ast::Distinct) -> Expr {
        let ty = self.lower_ty(distinct.ty(self.tree));

        Expr::Distinct {
            uid: self.uid_gen.generate_unique_id(),
            ty: self.twr_arena.alloc(ty),
        }
    }

    fn lower_binary_expr(&mut self, binary_expr: ast::BinaryExpr) -> Expr {
        let lhs = self.lower_expr(binary_expr.lhs(self.tree));
        let rhs = self.lower_expr(binary_expr.rhs(self.tree));

        let op = match binary_expr.op(self.tree) {
            Some(ast::BinaryOp::Add(_)) => BinaryOp::Add,
            Some(ast::BinaryOp::Sub(_)) => BinaryOp::Sub,
            Some(ast::BinaryOp::Mul(_)) => BinaryOp::Mul,
            Some(ast::BinaryOp::Div(_)) => BinaryOp::Div,
            Some(ast::BinaryOp::Lt(_)) => BinaryOp::Lt,
            Some(ast::BinaryOp::Gt(_)) => BinaryOp::Gt,
            Some(ast::BinaryOp::Le(_)) => BinaryOp::Le,
            Some(ast::BinaryOp::Ge(_)) => BinaryOp::Ge,
            Some(ast::BinaryOp::Eq(_)) => BinaryOp::Eq,
            Some(ast::BinaryOp::Ne(_)) => BinaryOp::Ne,
            Some(ast::BinaryOp::And(_)) => BinaryOp::And,
            Some(ast::BinaryOp::Or(_)) => BinaryOp::Or,
            None => return Expr::Missing,
        };

        Expr::Binary { lhs, rhs, op }
    }

    fn lower_unary_expr(&mut self, unary_expr: ast::UnaryExpr) -> Expr {
        let expr = self.lower_expr(unary_expr.expr(self.tree));

        let op = match unary_expr.op(self.tree) {
            Some(ast::UnaryOp::Pos(_)) => UnaryOp::Pos,
            Some(ast::UnaryOp::Neg(_)) => UnaryOp::Neg,
            Some(ast::UnaryOp::Not(_)) => UnaryOp::Not,
            None => return Expr::Missing,
        };

        Expr::Unary { expr, op }
    }

    fn lower_array_expr(&mut self, array_expr: ast::Array) -> Expr {
        let ty = self.lower_ty(array_expr.ty(self.tree));

        let body = match array_expr.body(self.tree) {
            Some(body) => body,
            None => {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::ArrayMissingBody,
                    range: TextRange::new(
                        array_expr.range(self.tree).end(),
                        array_expr
                            .range(self.tree)
                            .end()
                            .checked_add(TextSize::from(1))
                            .unwrap(),
                    ),
                });
                return Expr::Missing;
            }
        };

        let items = body
            .items(self.tree)
            .map(|item| self.lower_expr(item.value(self.tree)))
            .collect();

        Expr::Array { items, ty }
    }

    fn lower_block(&mut self, block: ast::Block) -> Expr {
        self.create_new_child_scope();

        let mut stmts = Vec::new();

        for stmt in block.stmts(self.tree) {
            let statement = self.lower_stmt(stmt);
            stmts.push(self.bodies.stmts.alloc(statement));
        }

        let tail_expr = block
            .tail_expr(self.tree)
            .map(|tail_expr| self.lower_expr(Some(tail_expr)));

        self.destroy_current_scope();

        Expr::Block { stmts, tail_expr }
    }

    fn lower_if(&mut self, if_expr: ast::IfExpr) -> Expr {
        let condition = self.lower_expr(if_expr.condition(self.tree));

        let body = self.lower_expr(if_expr.body(self.tree));

        let else_branch = if let Some(else_branch) = if_expr.else_branch(self.tree) {
            Some(self.lower_expr(else_branch.body(self.tree)))
        } else {
            None
        };

        Expr::If {
            condition,
            body,
            else_branch,
        }
    }

    fn lower_while(&mut self, while_expr: ast::WhileExpr) -> Expr {
        let condition = while_expr
            .condition(self.tree)
            .and_then(|condition| condition.value(self.tree))
            .map(|condition| self.lower_expr(Some(condition)));

        let body = self.lower_expr(while_expr.body(self.tree));

        Expr::While { condition, body }
    }

    fn lower_call(&mut self, call: ast::Call) -> Expr {
        let path = match call.name(self.tree) {
            Some(path) => path,
            None => return Expr::Missing,
        };

        let ident = match path.top_level_name(self.tree) {
            Some(ident) => ident,
            None => return Expr::Missing,
        };

        if let Some(function_name_token) = path.nested_name(self.tree) {
            let module_name_token = ident;

            let module_name = self.interner.intern(module_name_token.text(self.tree));
            let function_name = self.interner.intern(function_name_token.text(self.tree));

            let fqn = Fqn {
                module: Name(module_name),
                name: Name(function_name),
            };

            match self.world_index.get_definition(fqn) {
                Ok(definition) => {
                    let path = PathWithRange::OtherModule {
                        fqn,
                        module_range: module_name_token.range(self.tree),
                        name_range: function_name_token.range(self.tree),
                    };

                    self.bodies.other_module_references.insert(fqn);

                    self.bodies
                        .symbol_map
                        .insert(module_name_token, Symbol::Module(Name(module_name)));

                    match definition {
                        Definition::Function(function) => {
                            self.bodies
                                .symbol_map
                                .insert(function_name_token, Symbol::Function(path.path()));
                            return lower(self, call, function, path, function_name_token);
                        }
                        Definition::Global(_) => todo!(),
                        Definition::NamedTy(_) => todo!(),
                    }
                }
                Err(GetDefinitionError::UnknownModule) => {
                    self.diagnostics.push(LoweringDiagnostic {
                        kind: LoweringDiagnosticKind::UndefinedModule { name: module_name },
                        range: module_name_token.range(self.tree),
                    });

                    self.bodies
                        .symbol_map
                        .insert(module_name_token, Symbol::Unknown);
                    self.bodies
                        .symbol_map
                        .insert(function_name_token, Symbol::Unknown);

                    return Expr::Missing;
                }
                Err(GetDefinitionError::UnknownDefinition) => {
                    self.diagnostics.push(LoweringDiagnostic {
                        kind: LoweringDiagnosticKind::UndefinedLocal {
                            name: function_name,
                        },
                        range: function_name_token.range(self.tree),
                    });

                    self.bodies
                        .symbol_map
                        .insert(module_name_token, Symbol::Module(Name(module_name)));
                    self.bodies
                        .symbol_map
                        .insert(function_name_token, Symbol::Unknown);

                    return Expr::Missing;
                }
            }
        }

        // only have one ident as path
        let name = self.interner.intern(ident.text(self.tree));

        if let Some(def) = self.look_up_in_current_scope(name) {
            self.diagnostics.push(LoweringDiagnostic {
                kind: LoweringDiagnosticKind::CalledNonLambda { name },
                range: ident.range(self.tree),
            });

            self.bodies.symbol_map.insert(ident, Symbol::Local(def));
            return Expr::Local(def);
        }

        // todo: allow calling parameters
        if let Some((idx, ast)) = self.look_up_param(name) {
            self.diagnostics.push(LoweringDiagnostic {
                kind: LoweringDiagnosticKind::CalledNonLambda { name },
                range: ident.range(self.tree),
            });

            self.bodies.symbol_map.insert(ident, Symbol::Param(ast));
            return Expr::Param { idx };
        }

        let name = Name(name);
        if let Ok(definition) = self.world_index.get_definition(Fqn {
            module: self.module,
            name,
        }) {
            let path = PathWithRange::ThisModule {
                name,
                range: ident.range(self.tree),
            };

            match definition {
                Definition::Function(function) => {
                    self.bodies
                        .symbol_map
                        .insert(ident, Symbol::Function(path.path()));
                    return lower(self, call, function, path, ident);
                }
                Definition::Global(_) => {
                    self.diagnostics.push(LoweringDiagnostic {
                        kind: LoweringDiagnosticKind::CalledNonLambda { name: name.0 },
                        range: ident.range(self.tree),
                    });

                    self.bodies
                        .symbol_map
                        .insert(ident, Symbol::Global(path.path()));
                    return Expr::Global(path);
                }
                Definition::NamedTy(ty) => {
                    self.diagnostics.push(LoweringDiagnostic {
                        kind: LoweringDiagnosticKind::CalledNonLambda { name: name.0 },
                        range: ident.range(self.tree),
                    });

                    self.bodies
                        .symbol_map
                        .insert(ident, Symbol::Global(path.path()));
                    return Expr::Ty {
                        ty: self.twr_arena.alloc(*ty),
                    };
                }
            }
        }

        self.diagnostics.push(LoweringDiagnostic {
            kind: LoweringDiagnosticKind::UndefinedLocal { name: name.0 },
            range: ident.range(self.tree),
        });

        self.bodies.symbol_map.insert(ident, Symbol::Unknown);

        fn lower(
            ctx: &mut Ctx,
            call: ast::Call,
            function: &Function,
            path: PathWithRange,
            ident: ast::Ident,
        ) -> Expr {
            let arg_list = call.arg_list(ctx.tree);

            let expected = function.params.len() as u32;
            let got = match &arg_list {
                Some(al) => al.args(ctx.tree).count() as u32,
                None => 0,
            };

            if expected != got {
                let name = match path {
                    PathWithRange::ThisModule { name, .. } => name.0,
                    PathWithRange::OtherModule { fqn, .. } => fqn.name.0,
                };

                ctx.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::MismatchedArgCount {
                        name,
                        expected,
                        got,
                    },
                    range: ident.range(ctx.tree),
                });

                return Expr::Missing;
            }

            let mut args = Vec::new();

            if let Some(arg_list) = arg_list {
                for arg in arg_list.args(ctx.tree) {
                    let expr = ctx.lower_expr(arg.value(ctx.tree));
                    args.push(expr);
                }
            }

            Expr::Call { path, args }
        }

        return Expr::Missing;
    }

    fn lower_index_expr(&mut self, index_expr: ast::IndexExpr) -> Expr {
        let array = match index_expr.array(self.tree) {
            Some(array) => self.lower_expr(array.value(self.tree)),
            None => unreachable!(),
        };
        let index = match index_expr.index(self.tree) {
            Some(index) => self.lower_expr(index.value(self.tree)),
            None => unreachable!(),
        };

        Expr::Index { array, index }
    }

    fn lower_var_ref(&mut self, var_ref: ast::VarRef) -> Expr {
        let path = match var_ref.name(self.tree) {
            Some(path) => path,
            None => return Expr::Missing,
        };

        let ident = match path.top_level_name(self.tree) {
            Some(ident) => ident,
            None => return Expr::Missing,
        };

        if let Some(var_name_token) = path.nested_name(self.tree) {
            let module_name_token = ident;

            let module_name = self.interner.intern(module_name_token.text(self.tree));
            let var_name = self.interner.intern(var_name_token.text(self.tree));

            let fqn = Fqn {
                module: Name(module_name),
                name: Name(var_name),
            };

            match self.world_index.get_definition(fqn) {
                Ok(_) => {
                    let path = PathWithRange::OtherModule {
                        fqn,
                        module_range: module_name_token.range(self.tree),
                        name_range: var_name_token.range(self.tree),
                    };

                    self.bodies.other_module_references.insert(fqn);

                    self.bodies
                        .symbol_map
                        .insert(module_name_token, Symbol::Module(Name(module_name)));

                    self.bodies
                        .symbol_map
                        .insert(var_name_token, Symbol::Global(path.path()));

                    return Expr::Global(path);
                }
                Err(GetDefinitionError::UnknownModule) => {
                    self.diagnostics.push(LoweringDiagnostic {
                        kind: LoweringDiagnosticKind::UndefinedModule { name: module_name },
                        range: module_name_token.range(self.tree),
                    });

                    self.bodies
                        .symbol_map
                        .insert(module_name_token, Symbol::Unknown);
                    self.bodies
                        .symbol_map
                        .insert(var_name_token, Symbol::Unknown);

                    return Expr::Missing;
                }
                Err(GetDefinitionError::UnknownDefinition) => {
                    self.diagnostics.push(LoweringDiagnostic {
                        kind: LoweringDiagnosticKind::UndefinedLocal { name: var_name },
                        range: var_name_token.range(self.tree),
                    });

                    self.bodies
                        .symbol_map
                        .insert(module_name_token, Symbol::Module(Name(module_name)));
                    self.bodies
                        .symbol_map
                        .insert(var_name_token, Symbol::Unknown);

                    return Expr::Missing;
                }
            }
        }

        // only have one ident as path
        let name = self.interner.intern(ident.text(self.tree));

        if let Some(def) = self.look_up_in_current_scope(name) {
            self.bodies.symbol_map.insert(ident, Symbol::Local(def));
            return Expr::Local(def);
        }

        if let Some((idx, ast)) = self.look_up_param(name) {
            self.bodies.symbol_map.insert(ident, Symbol::Param(ast));
            return Expr::Param { idx };
        }

        let name = Name(name);
        if let Ok(_) = self.world_index.get_definition(Fqn {
            module: self.module,
            name,
        }) {
            let path = PathWithRange::ThisModule {
                name,
                range: ident.range(self.tree),
            };

            self.bodies
                .symbol_map
                .insert(ident, Symbol::Global(path.path()));

            return Expr::Global(path);
        }

        if let Some(ty) = TyWithRange::from_key(name.0, ident.range(self.tree)) {
            let ty = self.twr_arena.alloc(ty);

            self.bodies
                .symbol_map
                .insert(ident, Symbol::PrimitiveTy(ty));

            return Expr::Ty { ty };
        }

        self.diagnostics.push(LoweringDiagnostic {
            kind: LoweringDiagnosticKind::UndefinedLocal { name: name.0 },
            range: ident.range(self.tree),
        });

        self.bodies.symbol_map.insert(ident, Symbol::Unknown);

        return Expr::Missing;
    }

    fn lower_int_literal(&mut self, int_literal: ast::IntLiteral) -> Expr {
        let value = int_literal
            .value(self.tree)
            .and_then(|int| int.text(self.tree).parse().ok());

        if let Some(value) = value {
            return Expr::IntLiteral(value);
        }

        self.diagnostics.push(LoweringDiagnostic {
            kind: LoweringDiagnosticKind::OutOfRangeIntLiteral,
            range: int_literal.range(self.tree),
        });

        Expr::Missing
    }

    fn lower_bool_literal(&mut self, bool_literal: ast::BoolLiteral) -> Expr {
        let value = bool_literal
            .value(self.tree)
            .and_then(|b| b.text(self.tree).parse().ok());

        if let Some(value) = value {
            return Expr::BoolLiteral(value);
        }

        unreachable!()
    }

    fn lower_string_literal(&mut self, string_literal: ast::StringLiteral) -> Expr {
        let mut text = String::new();

        for component in string_literal.components(self.tree) {
            match component {
                ast::StringComponent::Escape(escape) => {
                    let escape_text = escape.text(self.tree);
                    let mut chars = escape_text.chars();
                    if cfg!(debug_assertions) {
                        assert_eq!(chars.next(), Some('\\'));
                    } else {
                        chars.next();
                    }

                    let escape_char = chars.next().unwrap();
                    debug_assert!(chars.next().is_none());

                    match escape_char {
                        '"' => text.push('"'),
                        '\\' => text.push('\\'),
                        'n' => text.push('\n'),
                        'r' => text.push('\r'),
                        't' => text.push('\t'),
                        _ => self.diagnostics.push(LoweringDiagnostic {
                            kind: LoweringDiagnosticKind::InvalidEscape,
                            range: escape.range(self.tree),
                        }),
                    }
                }
                ast::StringComponent::Contents(contents) => {
                    text.push_str(contents.text(self.tree));
                }
            }
        }

        Expr::StringLiteral(text)
    }

    fn insert_into_current_scope(&mut self, name: Key, value: Idx<LocalDef>) {
        let last_scope = self.scopes.last_mut().unwrap();
        last_scope.insert(name, value);
    }

    fn look_up_in_current_scope(&mut self, name: Key) -> Option<Idx<LocalDef>> {
        for scope in self.scopes.iter().rev() {
            if let Some(def) = scope.get(&name) {
                return Some(*def);
            }
        }

        None
    }

    fn look_up_param(&mut self, name: Key) -> Option<(u32, ast::Param)> {
        self.params.get(&name).copied()
    }

    fn create_new_child_scope(&mut self) {
        self.scopes.push(FxHashMap::default());
    }

    fn destroy_current_scope(&mut self) {
        self.scopes.pop();
    }
}

impl Bodies {
    pub fn function_body(&self, name: Name) -> Idx<Expr> {
        self.function_bodies[&name]
    }

    pub fn global(&self, name: Name) -> Idx<Expr> {
        self.globals[&name]
    }

    pub fn range_for_expr(&self, expr: Idx<Expr>) -> TextRange {
        self.expr_ranges[expr]
    }

    pub fn other_module_references(&self) -> &FxHashSet<Fqn> {
        &self.other_module_references
    }

    // todo: check if this is used
    pub fn symbol(&self, ident: ast::Ident) -> Option<Symbol> {
        self.symbol_map.get(&ident).copied()
    }

    fn shrink_to_fit(&mut self) {
        let Self {
            local_defs,
            stmts,
            exprs,
            function_bodies,
            other_module_references,
            symbol_map,
            ..
        } = self;

        local_defs.shrink_to_fit();
        stmts.shrink_to_fit();
        exprs.shrink_to_fit();
        //expr_ranges.shrink_to_fit();
        function_bodies.shrink_to_fit();
        other_module_references.shrink_to_fit();
        symbol_map.shrink_to_fit();
    }
}

impl std::ops::Index<Idx<LocalDef>> for Bodies {
    type Output = LocalDef;

    fn index(&self, id: Idx<LocalDef>) -> &Self::Output {
        &self.local_defs[id]
    }
}

impl std::ops::Index<Idx<LocalSet>> for Bodies {
    type Output = LocalSet;

    fn index(&self, id: Idx<LocalSet>) -> &Self::Output {
        &self.local_sets[id]
    }
}

impl std::ops::Index<Idx<Stmt>> for Bodies {
    type Output = Stmt;

    fn index(&self, id: Idx<Stmt>) -> &Self::Output {
        &self.stmts[id]
    }
}

impl std::ops::Index<Idx<Expr>> for Bodies {
    type Output = Expr;

    fn index(&self, id: Idx<Expr>) -> &Self::Output {
        &self.exprs[id]
    }
}

impl Bodies {
    pub fn debug(
        &self,
        module_name: &str,
        twr_arena: &Arena<TyWithRange>,
        interner: &Interner,
    ) -> String {
        let mut s = String::new();

        let mut globals: Vec<_> = self.globals.iter().collect();
        globals.sort_unstable_by_key(|(name, _)| *name);

        for (name, expr_id) in globals {
            s.push_str(&format!(
                "\n{}.{} := ",
                module_name,
                interner.lookup(name.0)
            ));
            write_expr(*expr_id, self, &mut s, twr_arena, interner, 0);
            s.push(';');
        }

        let mut function_bodies: Vec<_> = self.function_bodies.iter().collect();
        function_bodies.sort_unstable_by_key(|(name, _)| *name);

        for (name, expr_id) in function_bodies {
            s.push_str(&format!(
                "\n{}.{} := () -> ",
                module_name,
                interner.lookup(name.0)
            ));
            write_expr(*expr_id, self, &mut s, twr_arena, interner, 0);
            s.push(';');
        }

        if !self.other_module_references.is_empty() {
            let mut other_module_references: Vec<_> = self.other_module_references.iter().collect();
            other_module_references.sort_unstable();

            s.push_str(&format!(
                "\nReferences to other modules in {}:",
                module_name
            ));
            for fqn in &other_module_references {
                s.push_str(&format!(
                    "\n- {}.{}",
                    interner.lookup(fqn.module.0),
                    interner.lookup(fqn.name.0)
                ));
            }
        }

        return s.trim().to_string();

        fn write_expr(
            id: Idx<Expr>,
            bodies: &Bodies,
            s: &mut String,
            ty_arena: &Arena<TyWithRange>,
            interner: &Interner,
            mut indentation: usize,
        ) {
            match &bodies[id] {
                Expr::Missing => s.push_str("<missing>"),

                Expr::IntLiteral(n) => s.push_str(&format!("{}", n)),

                Expr::BoolLiteral(b) => s.push_str(&format!("{}", b)),

                Expr::StringLiteral(content) => s.push_str(&format!("{content:?}")),

                Expr::Array { items, ty } => {
                    s.push_str("[]");
                    s.push_str(ty.display(ty_arena, interner).as_str());
                    s.push('{');

                    for (idx, item) in items.iter().enumerate() {
                        s.push(' ');
                        write_expr(*item, bodies, s, ty_arena, interner, indentation);
                        if idx != items.len() - 1 {
                            s.push(',');
                        }
                    }

                    s.push_str(" }");
                }

                Expr::Index { array, index } => {
                    write_expr(*array, bodies, s, ty_arena, interner, indentation);
                    s.push_str("[ ");
                    write_expr(*index, bodies, s, ty_arena, interner, indentation);
                    s.push_str(" ]");
                }

                Expr::Cast { expr, ty } => {
                    write_expr(*expr, bodies, s, ty_arena, interner, indentation);

                    s.push_str(" as ");

                    s.push_str(ty.display(ty_arena, interner).as_str());
                }

                Expr::Ref { expr } => {
                    s.push_str("^");

                    write_expr(*expr, bodies, s, ty_arena, interner, indentation);
                }

                Expr::Deref { pointer } => {
                    write_expr(*pointer, bodies, s, ty_arena, interner, indentation);

                    s.push_str("^");
                }

                Expr::Binary { lhs, rhs, op } => {
                    write_expr(*lhs, bodies, s, ty_arena, interner, indentation);

                    s.push(' ');

                    match op {
                        BinaryOp::Add => s.push('+'),
                        BinaryOp::Sub => s.push('-'),
                        BinaryOp::Mul => s.push('*'),
                        BinaryOp::Div => s.push('/'),
                        BinaryOp::Lt => s.push('<'),
                        BinaryOp::Gt => s.push('>'),
                        BinaryOp::Le => s.push_str("<="),
                        BinaryOp::Ge => s.push_str(">="),
                        BinaryOp::Eq => s.push_str("=="),
                        BinaryOp::Ne => s.push_str("!="),
                        BinaryOp::And => s.push_str("&&"),
                        BinaryOp::Or => s.push_str("||"),
                    }

                    s.push(' ');

                    write_expr(*rhs, bodies, s, ty_arena, interner, indentation);
                }

                Expr::Unary { expr, op } => {
                    match op {
                        UnaryOp::Pos => s.push('+'),
                        UnaryOp::Neg => s.push('-'),
                        UnaryOp::Not => s.push('!'),
                    }

                    write_expr(*expr, bodies, s, ty_arena, interner, indentation);
                }

                Expr::Block {
                    stmts,
                    tail_expr: None,
                } if stmts.is_empty() => {
                    s.push_str("{}");
                }

                Expr::Block {
                    stmts,
                    tail_expr: Some(tail_expr),
                } if stmts.is_empty() => {
                    s.push_str("{ ");
                    write_expr(*tail_expr, bodies, s, ty_arena, interner, indentation + 4);
                    s.push_str(" }");
                }

                Expr::Block { stmts, tail_expr } => {
                    indentation += 4;

                    s.push_str("{\n");

                    for stmt in stmts.clone() {
                        s.push_str(&" ".repeat(indentation));
                        write_stmt(stmt, bodies, s, ty_arena, interner, indentation);
                        s.push('\n');
                    }

                    if let Some(tail_expr) = tail_expr {
                        s.push_str(&" ".repeat(indentation));
                        write_expr(*tail_expr, bodies, s, ty_arena, interner, indentation);
                        s.push('\n');
                    }

                    indentation -= 4;
                    s.push_str(&" ".repeat(indentation));

                    s.push('}');
                }

                Expr::If {
                    condition,
                    body,
                    else_branch,
                } => {
                    s.push_str("if ");
                    write_expr(*condition, bodies, s, ty_arena, interner, indentation);
                    s.push(' ');
                    write_expr(*body, bodies, s, ty_arena, interner, indentation);
                    if let Some(else_branch) = else_branch {
                        s.push_str(" else ");
                        write_expr(*else_branch, bodies, s, ty_arena, interner, indentation);
                    }
                }

                Expr::While { condition, body } => {
                    if let Some(condition) = condition {
                        s.push_str("while ");
                        write_expr(*condition, bodies, s, ty_arena, interner, indentation);
                        s.push(' ');
                    } else {
                        s.push_str("loop ");
                    }
                    write_expr(*body, bodies, s, ty_arena, interner, indentation);
                }

                Expr::Local(id) => s.push_str(&format!("l{}", id.into_raw())),

                Expr::Param { idx } => s.push_str(&format!("p{}", idx)),

                Expr::Call { path, args } => {
                    match path {
                        PathWithRange::ThisModule { name, .. } => {
                            s.push_str(interner.lookup(name.0))
                        }
                        PathWithRange::OtherModule { fqn, .. } => s.push_str(&format!(
                            "{}.{}",
                            interner.lookup(fqn.module.0),
                            interner.lookup(fqn.name.0)
                        )),
                    }

                    s.push_str("(");
                    for (idx, arg) in args.iter().enumerate() {
                        if idx != 0 {
                            s.push_str(", ");
                        }

                        write_expr(*arg, bodies, s, ty_arena, interner, indentation);
                    }
                    s.push_str(")");
                }

                Expr::Global(path) => match path {
                    PathWithRange::ThisModule { name, .. } => s.push_str(interner.lookup(name.0)),
                    PathWithRange::OtherModule { fqn, .. } => s.push_str(&format!(
                        "{}.{}",
                        interner.lookup(fqn.module.0),
                        interner.lookup(fqn.name.0)
                    )),
                },

                Expr::Ty { ty } => s.push_str(&ty_arena[*ty].display(ty_arena, interner)),

                Expr::Distinct { uid, ty } => {
                    s.push_str("distinct'");
                    s.push_str(&uid.to_string());
                    s.push(' ');
                    s.push_str(&ty_arena[*ty].display(ty_arena, interner));
                }
            }
        }

        fn write_stmt(
            id: Idx<Stmt>,
            bodies: &Bodies,
            s: &mut String,
            ty_arena: &Arena<TyWithRange>,
            interner: &Interner,
            indentation: usize,
        ) {
            match &bodies[id] {
                Stmt::Expr(expr_id) => {
                    write_expr(*expr_id, bodies, s, ty_arena, interner, indentation);
                    s.push(';');
                }
                Stmt::LocalDef(local_def_id) => {
                    s.push_str(&format!("l{} := ", local_def_id.into_raw()));
                    write_expr(
                        bodies[*local_def_id].value,
                        bodies,
                        s,
                        ty_arena,
                        interner,
                        indentation,
                    );
                    s.push(';');
                }
                Stmt::LocalSet(local_set_id) => {
                    s.push_str(&format!(
                        "l{} = ",
                        bodies[*local_set_id]
                            .local_def
                            .map_or("<unknown>".to_string(), |id| id.into_raw().to_string())
                    ));
                    write_expr(
                        bodies[*local_set_id].value,
                        bodies,
                        s,
                        ty_arena,
                        interner,
                        indentation,
                    );
                    s.push(';');
                }
            }
        }
    }
}
