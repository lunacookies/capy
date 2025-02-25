use std::{cmp::Ordering, env, mem, path::Path, vec};

use ast::{AstNode, AstToken};
use interner::{Interner, Key};
use la_arena::{Arena, ArenaMap, Idx};
use path_clean::PathClean;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::SyntaxTree;
use text_size::TextRange;

use crate::{subdir::SubDir, FileName, Fqn, Index, Name, NameWithRange, PrimitiveTy, UIDGenerator};

#[derive(Clone, Debug)]
pub struct Bodies {
    local_defs: Arena<LocalDef>,
    assigns: Arena<Assign>,
    stmts: Arena<Stmt>,
    exprs: Arena<Expr>,
    expr_ranges: ArenaMap<Idx<Expr>, TextRange>,
    global_tys: FxHashMap<Name, Idx<Expr>>,
    global_bodies: FxHashMap<Name, Idx<Expr>>,
    label_decls: bimap::BiMap<ScopeId, Idx<Expr>>,
    label_usages: FxHashMap<ScopeId, Vec<Idx<Stmt>>>,
    lambdas: Arena<Lambda>,
    comptimes: Arena<Comptime>,
    imports: FxHashSet<FileName>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Missing,
    IntLiteral(u64),
    FloatLiteral(f64),
    BoolLiteral(bool),
    StringLiteral(String),
    CharLiteral(u8),
    Cast {
        expr: Idx<Expr>,
        ty: Idx<Expr>,
    },
    Ref {
        mutable: bool,
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
        size: Option<u64>,
        items: Option<Vec<Idx<Expr>>>,
        ty: Idx<Expr>,
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
    LocalGlobal(NameWithRange),
    Param {
        idx: u32,
        range: TextRange,
    },
    Path {
        previous: Idx<Expr>,
        field: NameWithRange,
    },
    Call {
        callee: Idx<Expr>,
        args: Vec<Idx<Expr>>,
    },
    Lambda(Idx<Lambda>),
    Comptime(Idx<Comptime>),
    /// either a primitive type (such as `i32`, `bool`, etc.), or an array type,
    /// or a pointer to a primitive type, or a distinct type
    PrimitiveTy(PrimitiveTy),
    Distinct {
        uid: u32,
        ty: Idx<Expr>,
    },
    StructDecl {
        uid: u32,
        fields: Vec<(Option<NameWithRange>, Idx<Expr>)>,
    },
    StructLiteral {
        ty: Idx<Expr>,
        fields: Vec<(Option<NameWithRange>, Idx<Expr>)>,
    },
    Import(FileName),
}

#[derive(Debug, Clone)]
pub struct Lambda {
    pub params: Vec<Param>,
    pub params_range: TextRange,
    pub return_ty: Option<Idx<Expr>>,
    pub body: Idx<Expr>,
    pub is_extern: bool,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: Option<Name>,
    pub ty: Idx<Expr>,
}

#[derive(Debug, Clone, Copy)]
pub struct Comptime {
    pub body: Idx<Expr>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Idx<Expr>),
    LocalDef(Idx<LocalDef>),
    Assign(Idx<Assign>),
    Break {
        // `None` only for errors
        label: Option<ScopeId>,
        value: Option<Idx<Expr>>,
        range: TextRange,
    },
    Continue {
        // `None` only for errors
        label: Option<ScopeId>,
        range: TextRange,
    },
}

#[derive(Clone)]
pub struct LocalDef {
    pub mutable: bool,
    pub ty: Option<Idx<Expr>>,
    pub value: Idx<Expr>,
    pub ast: ast::Define,
    pub range: TextRange,
}

#[derive(Clone, Debug)]
pub struct Assign {
    pub source: Idx<Expr>,
    pub value: Idx<Expr>,
    pub range: TextRange,
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
    Mod,

    // cmp operations
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,

    // bitwise operations
    BAnd,
    BOr,
    Xor,
    LShift,
    RShift,

    // logical operations
    LAnd,
    LOr,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    // math operations
    Pos,
    Neg,

    // bitwise operations
    BNot,

    // logical operations
    LNot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoweringDiagnostic {
    pub kind: LoweringDiagnosticKind,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoweringDiagnosticKind {
    OutOfRangeIntLiteral,
    UndefinedRef { name: Key },
    UndefinedLabel { name: Key },
    NonGlobalExtern,
    ArraySizeNotConst,
    ArraySizeMismatch { found: u32, expected: u32 },
    InvalidEscape,
    TooManyCharsInCharLiteral,
    EmptyCharLiteral,
    NonU8CharLiteral,
    ModMustBeAlphanumeric,
    ModDoesNotExist { module: String, mod_dir: String },
    ModDoesNotContainModFile { module: String, mod_dir: String },
    ImportMustEndInDotCapy,
    ImportDoesNotExist { file: String },
    ImportOutsideCWD { file: String },
    ContinueNonLoop { name: Option<Key> },
}

#[allow(clippy::too_many_arguments)]
pub fn lower(
    root: ast::Root,
    tree: &SyntaxTree,
    file_name: &std::path::Path,
    index: &Index,
    uid_gen: &mut UIDGenerator,
    interner: &mut Interner,
    mod_dir: &Path,
    fake_file_system: bool,
) -> (Bodies, Vec<LoweringDiagnostic>) {
    let mut ctx = Ctx::new(
        file_name,
        index,
        uid_gen,
        interner,
        tree,
        mod_dir,
        fake_file_system,
    );

    for def in root.defs(tree) {
        ctx.lower_global(def.name(tree), def.ty(tree), def.value(tree))
    }

    ctx.bodies.shrink_to_fit();

    (ctx.bodies, ctx.diagnostics)
}

#[derive(PartialEq)]
enum ScopeKind {
    Block((Option<Key>, ScopeId)),
    Loop((Option<Key>, ScopeId)),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct ScopeId(u32);

impl ToString for ScopeId {
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}

struct Ctx<'a> {
    bodies: Bodies,
    file_name: &'a Path,
    index: &'a Index,
    uid_gen: &'a mut UIDGenerator,
    interner: &'a mut Interner,
    tree: &'a SyntaxTree,
    diagnostics: Vec<LoweringDiagnostic>,
    scopes: Vec<FxHashMap<Key, Idx<LocalDef>>>,
    label_kinds: Vec<ScopeKind>,
    label_gen: UIDGenerator,
    params: FxHashMap<Key, (u32, ast::Param)>,
    mod_dir: &'a Path,
    fake_file_system: bool, // used for importing files in tests
}

impl<'a> Ctx<'a> {
    fn new(
        file_name: &'a std::path::Path,
        index: &'a Index,
        uid_gen: &'a mut UIDGenerator,
        interner: &'a mut Interner,
        tree: &'a SyntaxTree,
        mod_dir: &'a Path,
        fake_file_system: bool,
    ) -> Self {
        Self {
            bodies: Bodies {
                local_defs: Arena::new(),
                assigns: Arena::new(),
                stmts: Arena::new(),
                exprs: Arena::new(),
                expr_ranges: ArenaMap::default(),
                global_tys: FxHashMap::default(),
                global_bodies: FxHashMap::default(),
                label_decls: bimap::BiMap::default(),
                label_usages: FxHashMap::default(),
                lambdas: Arena::new(),
                comptimes: Arena::new(),
                imports: FxHashSet::default(),
            },
            file_name,
            index,
            uid_gen,
            interner,
            tree,
            diagnostics: Vec::new(),
            scopes: vec![FxHashMap::default()],
            label_kinds: Vec::new(),
            label_gen: UIDGenerator::default(),
            params: FxHashMap::default(),
            mod_dir,
            fake_file_system,
        }
    }

    fn lower_global(
        &mut self,
        name_token: Option<ast::Ident>,
        ty_annotation: Option<ast::Ty>,
        expr: Option<ast::Expr>,
    ) {
        let name = match name_token {
            Some(ident) => Name(self.interner.intern(ident.text(self.tree))),
            None => return,
        };

        // if we’ve already seen a global with this name,
        // we ignore all other globals with that name
        //
        // we don’t have to worry about emitting a diagnostic here
        // because indexing already handles this
        if self.bodies.global_bodies.contains_key(&name) {
            return;
        }

        if let Some(ty) = ty_annotation {
            let ty = self.lower_expr(ty.expr(self.tree));

            self.bodies.global_tys.insert(name, ty);
        }

        let body = match expr {
            Some(ast::Expr::Lambda(lambda)) => {
                let body = self.lower_lambda(lambda, true);
                let body = self.bodies.exprs.alloc(body);

                self.bodies
                    .expr_ranges
                    .insert(body, expr.unwrap().range(self.tree));

                body
            }
            _ => self.lower_expr(expr),
        };
        self.bodies.global_bodies.insert(name, body);
    }

    fn lower_lambda(&mut self, lambda: ast::Lambda, allow_extern: bool) -> Expr {
        let old_labels = mem::take(&mut self.label_kinds);

        let mut params = Vec::new();
        let mut param_keys = FxHashMap::default();
        let mut param_type_ranges = Vec::new();

        if let Some(param_list) = lambda.param_list(self.tree) {
            for (idx, param) in param_list.params(self.tree).enumerate() {
                let key = param
                    .name(self.tree)
                    .map(|name| self.interner.intern(name.text(self.tree)));

                let ty = param.ty(self.tree);
                param_type_ranges.push(ty.map(|type_| type_.range(self.tree)));

                let ty = self.lower_expr(ty.and_then(|ty| ty.expr(self.tree)));

                params.push(Param {
                    name: key.map(Name),
                    ty,
                });

                if let Some(key) = key {
                    param_keys.insert(key, (idx as u32, param));
                }
            }
        }

        let return_ty = lambda
            .return_ty(self.tree)
            .and_then(|ty| ty.expr(self.tree))
            .map(|return_ty| self.lower_expr(Some(return_ty)));

        if !allow_extern {
            if let Some(r#extern) = lambda.r#extern(self.tree) {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::NonGlobalExtern,
                    range: r#extern.range(self.tree),
                });
            }
        }

        // todo: when parameter types are added, self.params should be cloned, and then updated in
        // place
        let old_params = mem::replace(&mut self.params, param_keys);
        let old_scopes = mem::take(&mut self.scopes);

        let body = self.lower_expr(lambda.body(self.tree));

        self.params = old_params;
        self.scopes = old_scopes;
        self.label_kinds = old_labels;

        Expr::Lambda(self.bodies.lambdas.alloc(Lambda {
            params,
            params_range: lambda.param_list(self.tree).unwrap().range(self.tree),
            return_ty,
            is_extern: lambda.r#extern(self.tree).is_some(),
            body,
        }))
    }

    fn lower_comptime(&mut self, comptime_expr: ast::ComptimeExpr) -> Expr {
        let old_params = mem::take(&mut self.params);
        let old_scopes = mem::take(&mut self.scopes);

        let body = self.lower_expr(comptime_expr.body(self.tree));

        self.params = old_params;
        self.scopes = old_scopes;

        Expr::Comptime(self.bodies.comptimes.alloc(Comptime { body }))
    }

    fn lower_stmt(&mut self, stmt: ast::Stmt) -> Stmt {
        match stmt {
            ast::Stmt::Define(local_def) => self.lower_local_define(local_def),
            ast::Stmt::Assign(local_set) => self.lower_assignment(local_set),
            ast::Stmt::Expr(expr_stmt) => {
                let expr = self.lower_expr(expr_stmt.expr(self.tree));
                Stmt::Expr(expr)
            }
            ast::Stmt::Return(return_stmt) => self.lower_return(return_stmt),
            ast::Stmt::Break(break_stmt) => self.lower_break(break_stmt),
            ast::Stmt::Continue(continue_stmt) => self.lower_continue(continue_stmt),
        }
    }

    fn lower_return(&mut self, return_stmt: ast::ReturnStmt) -> Stmt {
        let label = self.label_kinds.first().map(|kind| match kind {
            ScopeKind::Block((_, id)) => *id,
            // this should be unreachable, but you never know
            ScopeKind::Loop((_, id)) => *id,
        });

        Stmt::Break {
            label,
            value: return_stmt
                .value(self.tree)
                .map(|value| self.lower_expr(Some(value))),
            range: return_stmt.range(self.tree),
        }
    }

    fn lower_break(&mut self, break_stmt: ast::BreakStmt) -> Stmt {
        Stmt::Break {
            label: self.resolve_label(
                break_stmt.range(self.tree),
                break_stmt.label(self.tree),
                false,
            ),
            value: break_stmt
                .value(self.tree)
                .map(|value| self.lower_expr(Some(value))),
            range: break_stmt.range(self.tree),
        }
    }

    fn lower_continue(&mut self, continue_stmt: ast::ContinueStmt) -> Stmt {
        Stmt::Continue {
            label: self.resolve_label(
                continue_stmt.range(self.tree),
                continue_stmt.label(self.tree),
                true,
            ),
            range: continue_stmt.range(self.tree),
        }
    }

    fn resolve_label(
        &mut self,
        whole_range: TextRange,
        label: Option<ast::LabelRef>,
        require_loop: bool,
    ) -> Option<ScopeId> {
        let label_name = label
            .and_then(|label| label.name(self.tree))
            .map(|name| name.text(self.tree))
            .map(|name| self.interner.intern(name));

        if let Some(label_name) = label_name {
            for code in self.label_kinds.iter().rev() {
                match code {
                    ScopeKind::Block((Some(name), id)) if *name == label_name => {
                        if require_loop {
                            self.diagnostics.push(LoweringDiagnostic {
                                kind: LoweringDiagnosticKind::ContinueNonLoop { name: Some(*name) },
                                range: label.unwrap().range(self.tree),
                            });
                        }
                        return Some(*id);
                    }
                    ScopeKind::Loop((Some(name), id)) if *name == label_name => {
                        return Some(*id);
                    }
                    _ => continue,
                }
            }

            self.diagnostics.push(LoweringDiagnostic {
                kind: LoweringDiagnosticKind::UndefinedLabel { name: label_name },
                range: label.unwrap().range(self.tree),
            });

            return None;
        }

        for code in self.label_kinds.iter().rev() {
            match code {
                ScopeKind::Block((_, id)) if !require_loop => {
                    return Some(*id);
                }
                ScopeKind::Loop((_, id)) => {
                    return Some(*id);
                }
                _ => continue,
            }
        }

        self.diagnostics.push(LoweringDiagnostic {
            kind: if require_loop {
                LoweringDiagnosticKind::ContinueNonLoop { name: None }
            } else {
                unreachable!("breaks (statements) can only be inside blocks")
            },
            range: whole_range,
        });

        None
    }

    fn lower_local_define(&mut self, local_def: ast::Define) -> Stmt {
        let ty = local_def.ty(self.tree).and_then(|ty| ty.expr(self.tree));
        let ty = if ty.is_some() {
            Some(self.lower_expr(ty))
        } else {
            None
        };

        let value = self.lower_expr(local_def.value(self.tree));
        let id = self.bodies.local_defs.alloc(LocalDef {
            mutable: matches!(local_def, ast::Define::Variable(_)),
            ty,
            value,
            ast: local_def,
            range: local_def.range(self.tree),
        });

        if let Some(ident) = local_def.name(self.tree) {
            let name = self.interner.intern(ident.text(self.tree));
            self.insert_into_current_scope(name, id);
        }

        Stmt::LocalDef(id)
    }

    fn lower_assignment(&mut self, assign: ast::Assign) -> Stmt {
        let source = self.lower_expr(assign.source(self.tree).unwrap().value(self.tree));
        let value = self.lower_expr(assign.value(self.tree));

        let id = self.bodies.assigns.alloc(Assign {
            source,
            value,
            range: assign.range(self.tree),
            ast: assign,
        });

        Stmt::Assign(id)
    }

    fn lower_expr(&mut self, expr: Option<ast::Expr>) -> Idx<Expr> {
        let expr_ast = match expr {
            Some(expr) => expr,
            None => return self.bodies.exprs.alloc(Expr::Missing),
        };

        let range = expr_ast.range(self.tree);

        let (expr, scope_id) = self.lower_expr_raw(expr_ast);

        let id = self.bodies.exprs.alloc(expr);
        self.bodies.expr_ranges.insert(id, range);

        if scope_id.map_or(false, |id| self.bodies.label_usages.contains_key(&id)) {
            self.bodies.label_decls.insert(scope_id.unwrap(), id);
        }

        id
    }

    fn lower_expr_raw(&mut self, expr: ast::Expr) -> (Expr, Option<ScopeId>) {
        (
            match expr {
                ast::Expr::Cast(cast_expr) => self.lower_cast_expr(cast_expr),
                ast::Expr::Ref(ref_expr) => self.lower_ref_expr(ref_expr),
                ast::Expr::Deref(deref_expr) => self.lower_deref_expr(deref_expr),
                ast::Expr::Binary(binary_expr) => self.lower_binary_expr(binary_expr),
                ast::Expr::Unary(unary_expr) => self.lower_unary_expr(unary_expr),
                ast::Expr::Array(array_expr) => self.lower_array_expr(array_expr),
                ast::Expr::Block(block) => return self.lower_block(block, true),
                ast::Expr::If(if_expr) => self.lower_if(if_expr),
                ast::Expr::While(while_expr) => {
                    let res = self.lower_while(while_expr);
                    return (res.0, Some(res.1));
                }
                ast::Expr::Call(call) => self.lower_call(call),
                ast::Expr::IndexExpr(index_expr) => self.lower_index_expr(index_expr),
                ast::Expr::VarRef(var_ref) => self.lower_var_ref(var_ref),
                ast::Expr::Path(path) => self.lower_path(path),
                ast::Expr::IntLiteral(int_literal) => self.lower_int_literal(int_literal),
                ast::Expr::FloatLiteral(float_literal) => self.lower_float_literal(float_literal),
                ast::Expr::BoolLiteral(bool_literal) => self.lower_bool_literal(bool_literal),
                ast::Expr::CharLiteral(char_literal) => self.lower_char_literal(char_literal),
                ast::Expr::StringLiteral(string_literal) => {
                    self.lower_string_literal(string_literal)
                }
                ast::Expr::Distinct(distinct) => self.lower_distinct(distinct),
                ast::Expr::Lambda(lambda) => self.lower_lambda(lambda, false),
                ast::Expr::StructDecl(struct_decl) => self.lower_struct_declaration(struct_decl),
                ast::Expr::StructLiteral(struct_lit) => self.lower_struct_literal(struct_lit),
                ast::Expr::Import(import_expr) => self.lower_import(import_expr),
                ast::Expr::Comptime(comptime_expr) => self.lower_comptime(comptime_expr),
            },
            None,
        )
    }

    fn lower_cast_expr(&mut self, cast_expr: ast::CastExpr) -> Expr {
        let expr = self.lower_expr(cast_expr.expr(self.tree));
        let ty = self.lower_expr(cast_expr.ty(self.tree).and_then(|ty| ty.expr(self.tree)));

        Expr::Cast { expr, ty }
    }

    fn lower_ref_expr(&mut self, ref_expr: ast::RefExpr) -> Expr {
        let expr = self.lower_expr(ref_expr.expr(self.tree));

        Expr::Ref {
            mutable: ref_expr.mutable(self.tree).is_some(),
            expr,
        }
    }

    fn lower_deref_expr(&mut self, deref_expr: ast::DerefExpr) -> Expr {
        let pointer = self.lower_expr(deref_expr.pointer(self.tree));

        Expr::Deref { pointer }
    }

    fn lower_distinct(&mut self, distinct: ast::Distinct) -> Expr {
        let ty = self.lower_expr(distinct.ty(self.tree).and_then(|ty| ty.expr(self.tree)));

        Expr::Distinct {
            uid: self.uid_gen.generate_unique_id(),
            ty,
        }
    }

    fn lower_struct_declaration(&mut self, struct_decl: ast::StructDecl) -> Expr {
        let fields = struct_decl
            .fields(self.tree)
            .map(|field| {
                let name = field.name(self.tree).map(|ident| NameWithRange {
                    name: Name(self.interner.intern(ident.text(self.tree))),
                    range: ident.range(self.tree),
                });

                let ty = self.lower_expr(field.ty(self.tree).and_then(|ty| ty.expr(self.tree)));

                (name, ty)
            })
            .collect();

        Expr::StructDecl {
            uid: self.uid_gen.generate_unique_id(),
            fields,
        }
    }

    fn lower_struct_literal(&mut self, struct_lit: ast::StructLiteral) -> Expr {
        let ty = self.lower_expr(struct_lit.ty(self.tree).and_then(|ty| ty.expr(self.tree)));

        let mut fields = Vec::new();

        for field in struct_lit.fields(self.tree) {
            let name = field.name(self.tree).map(|ident| NameWithRange {
                name: Name(self.interner.intern(ident.text(self.tree))),
                range: ident.range(self.tree),
            });

            let value = self.lower_expr(field.value(self.tree));

            fields.push((name, value));
        }

        Expr::StructLiteral { ty, fields }
    }

    fn lower_import(&mut self, import: ast::ImportExpr) -> Expr {
        let file_name = match import.file(self.tree) {
            Some(file_name) => file_name,
            None => return Expr::Missing,
        };
        let old_diags_len = self.diagnostics.len();
        let file = match self.lower_string_literal(file_name) {
            Expr::StringLiteral(text) => text.replace(['/', '\\'], std::path::MAIN_SEPARATOR_STR),
            _ => unreachable!(),
        };
        if self.diagnostics.len() != old_diags_len {
            return Expr::Missing;
        }

        if import.r#mod(self.tree).is_some() {
            if !file.chars().all(|ch| ch.is_ascii_alphanumeric()) {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::ModMustBeAlphanumeric,
                    range: file_name.range(self.tree),
                });
                return Expr::Missing;
            }

            let mod_folder_path = self.mod_dir.join(&file);

            if !self.fake_file_system && !mod_folder_path.is_dir() {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::ModDoesNotExist {
                        module: file,
                        mod_dir: self.mod_dir.to_string_lossy().to_string(),
                    },
                    range: file_name.range(self.tree),
                });
                return Expr::Missing;
            }

            let mod_file_path = mod_folder_path.join("mod.capy").clean();

            if !self.fake_file_system && !mod_file_path.is_file() {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::ModDoesNotContainModFile {
                        module: file,
                        mod_dir: self.mod_dir.to_string_lossy().to_string(),
                    },
                    range: file_name.range(self.tree),
                });
                return Expr::Missing;
            }

            let mod_file_name = FileName(self.interner.intern(&mod_file_path.to_string_lossy()));

            // println!("{}", mod_file_path.display());
            // println!("{}", mod_file_name.0.to_raw());

            self.bodies.imports.insert(mod_file_name);
            return Expr::Import(mod_file_name);
        }

        if !file.ends_with(".capy") {
            self.diagnostics.push(LoweringDiagnostic {
                kind: LoweringDiagnosticKind::ImportMustEndInDotCapy,
                range: file_name.range(self.tree),
            });
            return Expr::Missing;
        }

        let file = if !self.fake_file_system {
            let file = std::path::Path::new(&file);

            let file = env::current_dir()
                .unwrap()
                .join(self.file_name)
                .join("..")
                .join(file)
                .clean();

            if !file.is_file() {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::ImportDoesNotExist {
                        file: file.to_string_lossy().to_string(),
                    },
                    range: file_name.range(self.tree),
                });
                return Expr::Missing;
            }

            if !file.is_sub_dir_of(self.mod_dir)
                && !file.is_sub_dir_of(&env::current_dir().unwrap())
            {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::ImportOutsideCWD {
                        file: file.to_string_lossy().to_string(),
                    },
                    range: file_name.range(self.tree),
                });
                return Expr::Missing;
            }

            file
        } else {
            file.into()
        };

        let file_name = FileName(self.interner.intern(&file.to_string_lossy()));

        // println!("{}", file.display());
        // println!("{}", file_name.0.to_raw());

        self.bodies.imports.insert(file_name);
        Expr::Import(file_name)
    }

    fn lower_binary_expr(&mut self, binary_expr: ast::BinaryExpr) -> Expr {
        let lhs = self.lower_expr(binary_expr.lhs(self.tree));
        let rhs = self.lower_expr(binary_expr.rhs(self.tree));

        let op = match binary_expr.op(self.tree) {
            Some(ast::BinaryOp::Add(_)) => BinaryOp::Add,
            Some(ast::BinaryOp::Sub(_)) => BinaryOp::Sub,
            Some(ast::BinaryOp::Mul(_)) => BinaryOp::Mul,
            Some(ast::BinaryOp::Div(_)) => BinaryOp::Div,
            Some(ast::BinaryOp::Mod(_)) => BinaryOp::Mod,
            Some(ast::BinaryOp::Lt(_)) => BinaryOp::Lt,
            Some(ast::BinaryOp::Gt(_)) => BinaryOp::Gt,
            Some(ast::BinaryOp::Le(_)) => BinaryOp::Le,
            Some(ast::BinaryOp::Ge(_)) => BinaryOp::Ge,
            Some(ast::BinaryOp::Eq(_)) => BinaryOp::Eq,
            Some(ast::BinaryOp::Ne(_)) => BinaryOp::Ne,
            Some(ast::BinaryOp::BAnd(_)) => BinaryOp::BAnd,
            Some(ast::BinaryOp::BOr(_)) => BinaryOp::BOr,
            Some(ast::BinaryOp::Xor(_)) => BinaryOp::Xor,
            Some(ast::BinaryOp::LShift(_)) => BinaryOp::LShift,
            Some(ast::BinaryOp::RShift(_)) => BinaryOp::RShift,
            Some(ast::BinaryOp::LAnd(_)) => BinaryOp::LAnd,
            Some(ast::BinaryOp::LOr(_)) => BinaryOp::LOr,
            None => return Expr::Missing,
        };

        Expr::Binary { lhs, rhs, op }
    }

    fn lower_unary_expr(&mut self, unary_expr: ast::UnaryExpr) -> Expr {
        let expr = self.lower_expr(unary_expr.expr(self.tree));

        let op = match unary_expr.op(self.tree) {
            Some(ast::UnaryOp::Pos(_)) => UnaryOp::Pos,
            Some(ast::UnaryOp::Neg(_)) => UnaryOp::Neg,
            Some(ast::UnaryOp::BNot(_)) => UnaryOp::BNot,
            Some(ast::UnaryOp::LNot(_)) => UnaryOp::LNot,
            None => return Expr::Missing,
        };

        Expr::Unary { expr, op }
    }

    fn lower_array_expr(&mut self, array_expr: ast::Array) -> Expr {
        let ty = self.lower_expr(array_expr.ty(self.tree).and_then(|ty| ty.expr(self.tree)));

        let items = array_expr.body(self.tree).map(|body| {
            body.items(self.tree)
                .map(|item| self.lower_expr(item.value(self.tree)))
                .collect::<Vec<_>>()
        });

        let items_len = items.as_ref().map(|items| items.len());
        let size = array_expr
            .size(self.tree)
            .and_then(|size| size.size(self.tree))
            .and_then(|size| match size {
                ast::Expr::IntLiteral(_) => Some(self.lower_expr_raw(size).0),
                other => {
                    self.diagnostics.push(LoweringDiagnostic {
                        kind: LoweringDiagnosticKind::ArraySizeNotConst,
                        range: other.range(self.tree),
                    });
                    None
                }
            })
            .and_then(|size| match (size, items_len) {
                (Expr::IntLiteral(size), Some(items_len)) => {
                    if size as usize != items_len {
                        self.diagnostics.push(LoweringDiagnostic {
                            kind: LoweringDiagnosticKind::ArraySizeMismatch {
                                found: items_len as u32,
                                expected: size as u32,
                            },
                            range: array_expr.body(self.tree).unwrap().range(self.tree),
                        });
                    }
                    Some(size)
                }
                (Expr::IntLiteral(size), None) => Some(size),
                _ => None,
            });

        Expr::Array { size, items, ty }
    }

    fn lower_block(&mut self, block: ast::Block, add_block_label: bool) -> (Expr, Option<ScopeId>) {
        let label_id = if add_block_label {
            let label_id = ScopeId(self.label_gen.generate_unique_id());
            let label_name = block
                .label(self.tree)
                .and_then(|label| label.name(self.tree))
                .map(|name| self.interner.intern(name.text(self.tree)));
            self.label_kinds
                .push(ScopeKind::Block((label_name, label_id)));
            Some(label_id)
        } else {
            None
        };

        self.create_new_child_scope();

        let mut stmts = Vec::new();

        for stmt in block.stmts(self.tree) {
            let statement = self.lower_stmt(stmt);

            let label_id = match statement {
                Stmt::Break { label, .. } => label,
                Stmt::Continue { label, .. } => label,
                _ => None,
            };

            let stmt_id = self.bodies.stmts.alloc(statement);
            stmts.push(stmt_id);

            if let Some(label_id) = label_id {
                self.bodies
                    .label_usages
                    .entry(label_id)
                    .or_default()
                    .push(stmt_id);
            }
        }

        let tail_expr = block
            .tail_expr(self.tree)
            .map(|tail_expr| self.lower_expr(Some(tail_expr)));

        self.destroy_current_scope();

        (Expr::Block { stmts, tail_expr }, label_id)
    }

    fn lower_if(&mut self, if_expr: ast::IfExpr) -> Expr {
        let condition = self.lower_expr(if_expr.condition(self.tree));

        let body = if let Some(ast::Expr::Block(body)) = if_expr.body(self.tree) {
            let range = body.range(self.tree);

            let (expr, _) = self.lower_block(body, false);

            let id = self.bodies.exprs.alloc(expr);
            self.bodies.expr_ranges.insert(id, range);

            id
        } else {
            self.bodies.exprs.alloc(Expr::Missing)
        };

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

    fn lower_while(&mut self, while_expr: ast::WhileExpr) -> (Expr, ScopeId) {
        let label_id = ScopeId(self.label_gen.generate_unique_id());
        let label_name = while_expr
            .label(self.tree)
            .and_then(|label| label.name(self.tree))
            .map(|name| self.interner.intern(name.text(self.tree)));
        self.label_kinds
            .push(ScopeKind::Loop((label_name, label_id)));

        let condition = while_expr
            .condition(self.tree)
            .and_then(|condition| condition.value(self.tree))
            .map(|condition| self.lower_expr(Some(condition)));

        let body = if let Some(ast::Expr::Block(body)) = while_expr.body(self.tree) {
            let range = body.range(self.tree);

            let (expr, _) = self.lower_block(body, false);

            let id = self.bodies.exprs.alloc(expr);
            self.bodies.expr_ranges.insert(id, range);

            id
        } else {
            self.bodies.exprs.alloc(Expr::Missing)
        };

        (Expr::While { condition, body }, label_id)
    }

    fn lower_call(&mut self, call: ast::Call) -> Expr {
        let callee = self.lower_expr(call.callee(self.tree));

        let mut args = Vec::new();

        if let Some(arg_list) = call.arg_list(self.tree) {
            for arg in arg_list.args(self.tree) {
                let expr = self.lower_expr(arg.value(self.tree));
                args.push(expr);
            }
        }

        Expr::Call { callee, args }
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

    fn lower_path(&mut self, path: ast::Path) -> Expr {
        let field = match path.field_name(self.tree) {
            Some(field) => field,
            None => return Expr::Missing,
        };
        let field_name = self.interner.intern(field.text(self.tree));

        let previous = path.previous_part(self.tree);

        Expr::Path {
            previous: self.lower_expr(previous),
            field: NameWithRange {
                name: Name(field_name),
                range: field.range(self.tree),
            },
        }
    }

    fn lower_var_ref(&mut self, var_ref: ast::VarRef) -> Expr {
        let ident = match var_ref.name(self.tree) {
            Some(ident) => ident,
            None => return Expr::Missing,
        };
        let ident_name = self.interner.intern(ident.text(self.tree));

        // only have one ident as path
        if let Some(def) = self.look_up_in_current_scope(ident_name) {
            return Expr::Local(def);
        }

        if let Some((idx, ast)) = self.look_up_param(ident_name) {
            return Expr::Param {
                idx,
                range: ast.range(self.tree),
            };
        }

        let name = Name(ident_name);
        if self.index.get_definition(name).is_some() {
            return Expr::LocalGlobal(NameWithRange {
                name,
                range: ident.range(self.tree),
            });
        }

        if let Some(ty) =
            PrimitiveTy::parse(Some(ast::Expr::VarRef(var_ref)), self.interner, self.tree)
        {
            return Expr::PrimitiveTy(ty);
        }

        self.diagnostics.push(LoweringDiagnostic {
            kind: LoweringDiagnosticKind::UndefinedRef { name: name.0 },
            range: ident.range(self.tree),
        });

        Expr::Missing
    }

    fn lower_int_literal(&mut self, int_literal: ast::IntLiteral) -> Expr {
        let Some(value) = int_literal.value(self.tree) else {
            return Expr::Missing;
        };
        let value = value.text(self.tree).replace('_', "");
        let mut value = value.split(['e', 'E']);

        // there will always be a first part
        let Ok(base) = value.next().unwrap().parse::<u64>() else {
            self.diagnostics.push(LoweringDiagnostic {
                kind: LoweringDiagnosticKind::OutOfRangeIntLiteral,
                range: int_literal.range(self.tree),
            });
            return Expr::Missing;
        };

        let val = if let Some(e) = value.next() {
            let Some(result) = e
                .parse()
                .ok()
                .and_then(|e| 10_u64.checked_pow(e))
                .and_then(|e| base.checked_mul(e))
            else {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::OutOfRangeIntLiteral,
                    range: int_literal.range(self.tree),
                });
                return Expr::Missing;
            };

            result
        } else {
            base
        };

        Expr::IntLiteral(val)
    }

    fn lower_float_literal(&mut self, float_literal: ast::FloatLiteral) -> Expr {
        let value = float_literal
            .value(self.tree)
            .and_then(|int| int.text(self.tree).replace('_', "").parse().ok())
            .unwrap();

        Expr::FloatLiteral(value)
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
                        '0' => text.push('\0'),   // null
                        'a' => text.push('\x07'), // bell (BEL)
                        'b' => text.push('\x08'), // backspace
                        'n' => text.push('\n'),   // line feed (new line)
                        'f' => text.push('\x0C'), // form feed (new page)
                        'r' => text.push('\r'),   // carraige return
                        't' => text.push('\t'),   // horizontal tab
                        'v' => text.push('\x0B'), // vertical tab
                        'e' => text.push('\x1B'), // escape
                        '"' => text.push('"'),
                        '\'' => text.push('\''),
                        '\\' => text.push('\\'),
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

    fn lower_char_literal(&mut self, char_literal: ast::CharLiteral) -> Expr {
        let mut text = String::new();

        let mut total_len = 0;
        for component in char_literal.components(self.tree) {
            match component {
                ast::StringComponent::Escape(escape) => {
                    // we do this instead of text.len() because just below
                    // an escape sequence has the chance to add nothing to text
                    total_len += 1;

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
                        '0' => text.push('\0'),   // null
                        'a' => text.push('\x07'), // bell (BEL)
                        'b' => text.push('\x08'), // backspace
                        'n' => text.push('\n'),   // line feed (new line)
                        'f' => text.push('\x0C'), // form feed (new page)
                        'r' => text.push('\r'),   // carraige return
                        't' => text.push('\t'),   // horizontal tab
                        'v' => text.push('\x0B'), // vertical tab
                        'e' => text.push('\x1B'), // escape
                        '\'' => text.push('\''),
                        '"' => text.push('"'),
                        '\\' => text.push('\\'),
                        _ => self.diagnostics.push(LoweringDiagnostic {
                            kind: LoweringDiagnosticKind::InvalidEscape,
                            range: escape.range(self.tree),
                        }),
                    }
                }
                ast::StringComponent::Contents(contents) => {
                    let contents = contents.text(self.tree);

                    total_len += contents.chars().count();
                    text.push_str(contents);
                }
            }
        }

        let ch = match total_len.cmp(&1) {
            Ordering::Less => {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::EmptyCharLiteral,
                    range: char_literal.range(self.tree),
                });

                0
            }
            Ordering::Equal => text
                .chars()
                .next()
                .unwrap_or('\0')
                .try_into()
                .unwrap_or_else(|_| {
                    self.diagnostics.push(LoweringDiagnostic {
                        kind: LoweringDiagnosticKind::NonU8CharLiteral,
                        range: char_literal.range(self.tree),
                    });

                    0
                }),
            Ordering::Greater => {
                self.diagnostics.push(LoweringDiagnostic {
                    kind: LoweringDiagnosticKind::TooManyCharsInCharLiteral,
                    range: char_literal.range(self.tree),
                });

                0
            }
        };

        Expr::CharLiteral(ch)
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
    pub fn has_global(&self, name: Name) -> bool {
        self.global_bodies.contains_key(&name)
    }

    pub fn global_body(&self, name: Name) -> Idx<Expr> {
        self.global_bodies[&name]
    }

    pub fn global_ty(&self, name: Name) -> Option<Idx<Expr>> {
        self.global_tys.get(&name).copied()
    }

    pub fn range_for_expr(&self, expr: Idx<Expr>) -> TextRange {
        self.expr_ranges[expr]
    }

    pub fn range_for_stmt(&self, stmt: Idx<Stmt>) -> TextRange {
        match self.stmts[stmt] {
            Stmt::Expr(expr) => self.range_for_expr(expr),
            Stmt::LocalDef(def) => self.local_defs[def].range,
            Stmt::Assign(assign) => self.assigns[assign].range,
            Stmt::Break { range, .. } => range,
            Stmt::Continue { range, .. } => range,
        }
    }

    pub fn comptimes(&self) -> impl Iterator<Item = Idx<Comptime>> + '_ {
        self.comptimes.iter().map(|(idx, _)| idx)
    }

    pub fn imports(&self) -> &FxHashSet<FileName> {
        &self.imports
    }

    /// only blocks which are actually `break`d or `continue`d out of will get a scopeid
    pub fn block_to_scope_id(&self, expr: Idx<Expr>) -> Option<ScopeId> {
        self.label_decls.get_by_right(&expr).copied()
    }

    pub fn scope_id_to_block(&self, id: ScopeId) -> Idx<Expr> {
        *self.label_decls.get_by_left(&id).unwrap()
    }

    // a `ScopeId` will only be stored if it has usages
    pub fn scope_id_usages(&self, id: ScopeId) -> &Vec<Idx<Stmt>> {
        self.label_usages.get(&id).unwrap()
    }

    fn shrink_to_fit(&mut self) {
        let Self {
            local_defs,
            stmts,
            exprs,
            assigns,
            expr_ranges: _,
            global_tys,
            global_bodies,
            label_decls,
            label_usages,
            lambdas,
            comptimes,
            imports,
        } = self;

        local_defs.shrink_to_fit();
        stmts.shrink_to_fit();
        exprs.shrink_to_fit();
        assigns.shrink_to_fit();
        global_tys.shrink_to_fit();
        global_bodies.shrink_to_fit();
        lambdas.shrink_to_fit();
        comptimes.shrink_to_fit();
        imports.shrink_to_fit();
        label_decls.shrink_to_fit();
        label_usages.shrink_to_fit()
    }
}

impl std::ops::Index<Idx<LocalDef>> for Bodies {
    type Output = LocalDef;

    fn index(&self, id: Idx<LocalDef>) -> &Self::Output {
        &self.local_defs[id]
    }
}

impl std::ops::Index<ScopeId> for Bodies {
    type Output = Idx<Expr>;

    fn index(&self, id: ScopeId) -> &Self::Output {
        self.label_decls.get_by_left(&id).unwrap()
    }
}

impl std::ops::Index<Idx<Assign>> for Bodies {
    type Output = Assign;

    fn index(&self, id: Idx<Assign>) -> &Self::Output {
        &self.assigns[id]
    }
}

impl std::ops::Index<Idx<Lambda>> for Bodies {
    type Output = Lambda;

    fn index(&self, id: Idx<Lambda>) -> &Self::Output {
        &self.lambdas[id]
    }
}

impl std::ops::Index<Idx<Comptime>> for Bodies {
    type Output = Comptime;

    fn index(&self, id: Idx<Comptime>) -> &Self::Output {
        &self.comptimes[id]
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
        file: FileName,
        mod_dir: &std::path::Path,
        interner: &Interner,
        show_expr_idx: bool,
    ) -> String {
        let mut s = String::new();

        let mut globals: Vec<_> = self.global_bodies.iter().collect();
        globals.sort_unstable_by_key(|(name, _)| *name);

        for (name, expr_id) in globals {
            s.push_str(&format!(
                "{} :: ",
                Fqn { file, name: *name }.to_string(mod_dir, interner)
            ));
            write_expr(&mut s, *expr_id, show_expr_idx, self, mod_dir, interner, 0);
            s.push_str(";\n");
        }

        return s;

        #[allow(clippy::too_many_arguments)]
        fn write_expr(
            s: &mut String,
            idx: Idx<Expr>,
            show_idx: bool,
            bodies: &Bodies,
            mod_dir: &std::path::Path,
            interner: &Interner,
            mut indentation: usize,
        ) {
            if show_idx {
                s.push_str("\x1B[90m(\x1B[0m")
            }

            match &bodies[idx] {
                Expr::Missing => s.push_str("<missing>"),

                Expr::IntLiteral(n) => s.push_str(&format!("{}", n)),

                Expr::FloatLiteral(n) => s.push_str(&format!("{}", n)),

                Expr::BoolLiteral(b) => s.push_str(&format!("{}", b)),

                Expr::StringLiteral(content) => s.push_str(&format!("{content:?}")),

                Expr::CharLiteral(char) => s.push_str(&format!("{:?}", Into::<char>::into(*char))),

                Expr::Array { size, items, ty } => {
                    s.push('[');
                    if let Some(size) = size {
                        s.push_str(&size.to_string());
                    }
                    s.push(']');
                    write_expr(s, *ty, show_idx, bodies, mod_dir, interner, indentation);

                    if let Some(items) = items {
                        s.push('{');

                        for (idx, item) in items.iter().enumerate() {
                            s.push(' ');
                            write_expr(s, *item, show_idx, bodies, mod_dir, interner, indentation);
                            if idx != items.len() - 1 {
                                s.push(',');
                            }
                        }

                        s.push_str(" }");
                    }
                }

                Expr::Index { array, index } => {
                    write_expr(s, *array, show_idx, bodies, mod_dir, interner, indentation);
                    s.push('[');
                    write_expr(s, *index, show_idx, bodies, mod_dir, interner, indentation);
                    s.push(']');
                }

                Expr::Cast { expr, ty } => {
                    write_expr(s, *expr, show_idx, bodies, mod_dir, interner, indentation);

                    s.push_str(" as ");

                    write_expr(s, *ty, show_idx, bodies, mod_dir, interner, indentation);
                }

                Expr::Ref { mutable, expr } => {
                    s.push('^');

                    if *mutable {
                        s.push_str("mut ");
                    }

                    write_expr(s, *expr, show_idx, bodies, mod_dir, interner, indentation);
                }

                Expr::Deref { pointer } => {
                    write_expr(
                        s,
                        *pointer,
                        show_idx,
                        bodies,
                        mod_dir,
                        interner,
                        indentation,
                    );

                    s.push('^');
                }

                Expr::Binary { lhs, rhs, op } => {
                    write_expr(s, *lhs, show_idx, bodies, mod_dir, interner, indentation);

                    s.push(' ');

                    match op {
                        BinaryOp::Add => s.push('+'),
                        BinaryOp::Sub => s.push('-'),
                        BinaryOp::Mul => s.push('*'),
                        BinaryOp::Div => s.push('/'),
                        BinaryOp::Mod => s.push('%'),
                        BinaryOp::Lt => s.push('<'),
                        BinaryOp::Gt => s.push('>'),
                        BinaryOp::Le => s.push_str("<="),
                        BinaryOp::Ge => s.push_str(">="),
                        BinaryOp::Eq => s.push_str("=="),
                        BinaryOp::Ne => s.push_str("!="),
                        BinaryOp::BAnd => s.push('&'),
                        BinaryOp::BOr => s.push('|'),
                        BinaryOp::Xor => s.push('~'),
                        BinaryOp::LShift => s.push_str("<<"),
                        BinaryOp::RShift => s.push_str(">>"),
                        BinaryOp::LAnd => s.push_str("&&"),
                        BinaryOp::LOr => s.push_str("||"),
                    }

                    s.push(' ');

                    write_expr(s, *rhs, show_idx, bodies, mod_dir, interner, indentation);
                }

                Expr::Unary { expr, op } => {
                    match op {
                        UnaryOp::Pos => s.push('+'),
                        UnaryOp::Neg => s.push('-'),
                        UnaryOp::BNot => s.push('~'),
                        UnaryOp::LNot => s.push('!'),
                    }

                    write_expr(s, *expr, show_idx, bodies, mod_dir, interner, indentation);
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
                    if let Some(label_id) = bodies.label_decls.get_by_right(&idx) {
                        s.push('`');
                        s.push_str(&label_id.to_string());
                        s.push(' ');
                    }

                    let mut inner = String::new();
                    write_expr(
                        &mut inner,
                        *tail_expr,
                        show_idx,
                        bodies,
                        mod_dir,
                        interner,
                        indentation + 4,
                    );

                    if inner.len() > 60 {
                        s.push_str("{\n");
                        s.push_str(&" ".repeat(indentation + 4));
                    } else {
                        s.push_str("{ ");
                    }

                    s.push_str(&inner);

                    if inner.len() > 60 {
                        s.push('\n');

                        s.push_str(&" ".repeat(indentation));

                        s.push('}');
                    } else {
                        s.push_str(" }");
                    }
                }

                Expr::Block { stmts, tail_expr } => {
                    indentation += 4;

                    if let Some(label_id) = bodies.label_decls.get_by_right(&idx) {
                        s.push('`');
                        s.push_str(&label_id.to_string());
                        s.push(' ');
                    }

                    s.push_str("{\n");

                    for stmt in stmts.clone() {
                        s.push_str(&" ".repeat(indentation));
                        write_stmt(s, stmt, show_idx, bodies, mod_dir, interner, indentation);
                        s.push('\n');
                    }

                    if let Some(tail_expr) = tail_expr {
                        s.push_str(&" ".repeat(indentation));
                        write_expr(
                            s,
                            *tail_expr,
                            show_idx,
                            bodies,
                            mod_dir,
                            interner,
                            indentation,
                        );
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
                    write_expr(
                        s,
                        *condition,
                        show_idx,
                        bodies,
                        mod_dir,
                        interner,
                        indentation,
                    );
                    s.push(' ');
                    write_expr(s, *body, show_idx, bodies, mod_dir, interner, indentation);
                    if let Some(else_branch) = else_branch {
                        s.push_str(" else ");
                        write_expr(
                            s,
                            *else_branch,
                            show_idx,
                            bodies,
                            mod_dir,
                            interner,
                            indentation,
                        );
                    }
                }

                Expr::While { condition, body } => {
                    if let Some(label_id) = bodies.label_decls.get_by_right(&idx) {
                        s.push('`');
                        s.push_str(&label_id.to_string());
                        s.push(' ');
                    }

                    if let Some(condition) = condition {
                        s.push_str("while ");
                        write_expr(
                            s,
                            *condition,
                            show_idx,
                            bodies,
                            mod_dir,
                            interner,
                            indentation,
                        );
                        s.push(' ');
                    } else {
                        s.push_str("loop ");
                    }
                    write_expr(s, *body, show_idx, bodies, mod_dir, interner, indentation);
                }

                Expr::Local(id) => s.push_str(&format!("l{}", id.into_raw())),

                Expr::Param { idx, .. } => s.push_str(&format!("p{}", idx)),

                Expr::Call { callee, args } => {
                    write_expr(s, *callee, show_idx, bodies, mod_dir, interner, indentation);

                    s.push('(');
                    for (idx, arg) in args.iter().enumerate() {
                        if idx != 0 {
                            s.push_str(", ");
                        }

                        write_expr(s, *arg, show_idx, bodies, mod_dir, interner, indentation);
                    }
                    s.push(')');
                }

                Expr::LocalGlobal(name) => s.push_str(interner.lookup(name.name.0)),

                Expr::Path {
                    previous, field, ..
                } => {
                    write_expr(
                        s,
                        *previous,
                        show_idx,
                        bodies,
                        mod_dir,
                        interner,
                        indentation,
                    );

                    s.push('.');

                    s.push_str(interner.lookup(field.name.0));
                }

                Expr::Lambda(lambda) => {
                    let Lambda {
                        params,
                        return_ty,
                        body,
                        is_extern,
                        ..
                    } = &bodies.lambdas[*lambda];

                    s.push('(');
                    for (idx, param) in params.iter().enumerate() {
                        s.push('p');
                        s.push_str(idx.to_string().as_str());
                        s.push_str(": ");

                        write_expr(
                            s,
                            param.ty,
                            show_idx,
                            bodies,
                            mod_dir,
                            interner,
                            indentation,
                        );

                        if idx != params.len() - 1 {
                            s.push_str(", ");
                        }
                    }
                    s.push_str(") ");

                    if let Some(return_ty) = return_ty {
                        s.push_str("-> ");

                        write_expr(
                            s,
                            *return_ty,
                            show_idx,
                            bodies,
                            mod_dir,
                            interner,
                            indentation,
                        );

                        s.push(' ');
                    }

                    if *is_extern {
                        s.push_str("extern");
                    } else {
                        write_expr(s, *body, show_idx, bodies, mod_dir, interner, indentation);
                    }
                }

                Expr::Comptime(comptime) => {
                    let Comptime { body } = bodies.comptimes[*comptime];

                    s.push_str("comptime ");

                    write_expr(s, body, show_idx, bodies, mod_dir, interner, indentation);
                }

                Expr::StructLiteral { ty, fields } => {
                    write_expr(s, *ty, show_idx, bodies, mod_dir, interner, indentation);

                    s.push_str(" {");

                    for (idx, (name, value)) in fields.iter().enumerate() {
                        if let Some(name) = name {
                            s.push_str(interner.lookup(name.name.0));
                            s.push_str(": ");
                        }

                        write_expr(s, *value, show_idx, bodies, mod_dir, interner, indentation);

                        if idx != fields.len() - 1 {
                            s.push_str(", ");
                        }
                    }

                    s.push('}');
                }

                Expr::PrimitiveTy(ty) => s.push_str(&ty.display()),

                Expr::Distinct { uid, ty } => {
                    s.push_str("distinct'");
                    s.push_str(&uid.to_string());
                    s.push(' ');
                    write_expr(s, *ty, show_idx, bodies, mod_dir, interner, indentation);
                }

                Expr::StructDecl { uid, fields } => {
                    s.push_str("struct'");
                    s.push_str(&uid.to_string());
                    s.push_str(" {");
                    for (idx, (name, ty)) in fields.iter().enumerate() {
                        s.push(' ');
                        if let Some(name) = name {
                            s.push_str(interner.lookup(name.name.0));
                        } else {
                            s.push('?');
                        }
                        s.push(':');
                        write_expr(s, *ty, show_idx, bodies, mod_dir, interner, indentation);
                        if idx != fields.len() - 1 {
                            s.push(',');
                        }
                    }
                    s.push_str(" }");
                }

                Expr::Import(file_name) => {
                    s.push_str(&format!(r#"import "{}""#, interner.lookup(file_name.0)))
                }
            }

            if show_idx {
                s.push_str("\x1B[90m #");
                s.push_str(&idx.into_raw().to_string());
                s.push_str(")\x1B[0m")
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn write_stmt(
            s: &mut String,
            expr: Idx<Stmt>,
            show_idx: bool,
            bodies: &Bodies,
            mod_dir: &std::path::Path,
            interner: &Interner,
            indentation: usize,
        ) {
            match &bodies[expr] {
                Stmt::Expr(expr_id) => {
                    write_expr(
                        s,
                        *expr_id,
                        show_idx,
                        bodies,
                        mod_dir,
                        interner,
                        indentation,
                    );
                    s.push(';');
                }
                Stmt::LocalDef(local_def_id) => {
                    s.push_str(&format!("l{} :", local_def_id.into_raw()));

                    let local_def = &bodies[*local_def_id];

                    if let Some(ty) = local_def.ty {
                        s.push(' ');
                        write_expr(s, ty, show_idx, bodies, mod_dir, interner, indentation);
                        s.push(' ');
                    }

                    s.push_str("= ");

                    write_expr(
                        s,
                        local_def.value,
                        show_idx,
                        bodies,
                        mod_dir,
                        interner,
                        indentation,
                    );
                    s.push(';');
                }
                Stmt::Assign(local_set_id) => {
                    write_expr(
                        s,
                        bodies[*local_set_id].source,
                        show_idx,
                        bodies,
                        mod_dir,
                        interner,
                        indentation,
                    );
                    s.push_str(" = ");
                    write_expr(
                        s,
                        bodies[*local_set_id].value,
                        show_idx,
                        bodies,
                        mod_dir,
                        interner,
                        indentation,
                    );
                    s.push(';');
                }
                Stmt::Break { label, value, .. } => {
                    s.push_str("break ");
                    if let Some(label) = label {
                        s.push_str(&label.to_string());
                    } else {
                        s.push_str("<unknown>");
                    }
                    s.push('`');
                    if let Some(value) = value {
                        s.push(' ');
                        write_expr(s, *value, show_idx, bodies, mod_dir, interner, indentation);
                    }
                    s.push(';');
                }
                Stmt::Continue { label, .. } => {
                    s.push_str("continue ");
                    if let Some(label) = label {
                        s.push_str(&label.to_string());
                    } else {
                        s.push_str("<unknown>")
                    }
                    s.push('`');
                    s.push(';');
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::{expect, Expect};

    fn check<const N: usize>(
        input: &str,
        expect: Expect,
        expected_diagnostics: impl Fn(
            &mut Interner,
        ) -> [(LoweringDiagnosticKind, std::ops::Range<u32>); N],
    ) {
        let mut interner = Interner::default();
        let mut uid_gen = UIDGenerator::default();

        let tokens = lexer::lex(input);
        let tree = parser::parse_source_file(&tokens, input).into_syntax_tree();
        let root = ast::Root::cast(tree.root(), &tree).unwrap();
        let (index, _) = crate::index(root, &tree, &mut interner);

        let (bodies, actual_diagnostics) = lower(
            root,
            &tree,
            Path::new("main.capy"),
            &index,
            &mut uid_gen,
            &mut interner,
            Path::new("/capy/modules"),
            true,
        );

        expect.assert_eq(&bodies.debug(
            FileName(interner.intern("main.capy")),
            std::path::Path::new(""),
            &interner,
            false,
        ));

        let expected_diagnostics: Vec<_> = expected_diagnostics(&mut interner)
            .into_iter()
            .map(|(kind, range)| LoweringDiagnostic {
                kind,
                range: TextRange::new(range.start.into(), range.end.into()),
            })
            .collect();

        assert_eq!(expected_diagnostics, actual_diagnostics);
    }

    #[test]
    fn empty() {
        check("", expect![""], |_| [])
    }

    #[test]
    fn function() {
        check(
            r#"
                foo :: () {
                    
                }
            "#,
            expect![[r#"
                main::foo :: () {};
            "#]],
            |_| [],
        )
    }

    #[test]
    fn binary() {
        check(
            r#"
                foo :: () {
                    1 + 1;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    1 + 1;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn global() {
        check(
            r#"
                foo :: 5;

                bar :: () {
                    foo;
                }
            "#,
            expect![[r#"
                main::foo :: 5;
                main::bar :: () {
                    foo;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn local_var() {
        check(
            r#"
                foo :: () {
                    x := 5;

                    x;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := 5;
                    l0;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn param() {
        check(
            r#"
                foo :: (x: i32) {
                    x;
                }
            "#,
            expect![[r#"
                main::foo :: (p0: i32) {
                    p0;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn import() {
        check(
            r#"
                other_file :: import "other_file.capy";

                foo :: () {
                    other_file.global;
                }
            "#,
            expect![[r#"
                main::other_file :: import "other_file.capy";
                main::foo :: () {
                    other_file.global;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn import_non_dot_capy() {
        check(
            r#"
                foo :: () {
                    other_file :: import "other_file.cap";

                    other_file.global;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := <missing>;
                    l0.global;
                };
            "#]],
            |_| [(LoweringDiagnosticKind::ImportMustEndInDotCapy, 70..86)],
        )
    }

    #[test]
    fn int_literal() {
        check(
            r#"
                foo :: () {
                    num := 18446744073709551615;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := 18446744073709551615;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn int_literal_with_e_lower() {
        check(
            r#"
                foo :: () {
                    // 123 * 10^9
                    num := 1_23_e9_;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := 123000000000;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn int_literal_with_e_upper() {
        check(
            r#"
                foo :: () {
                    // 456... * 10^(-10)
                    num := 4_5_6_E1_0_;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := 4560000000000;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn int_literal_with_e_very_large() {
        check(
            r#"
                foo :: () {
                    num := 1e20;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := <missing>;
                };
            "#]],
            |_| [(LoweringDiagnosticKind::OutOfRangeIntLiteral, 56..60)],
        )
    }

    #[test]
    fn out_of_range_int_literal() {
        check(
            r#"
                foo :: () {
                    num := 18446744073709551616;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := <missing>;
                };
            "#]],
            |_| [(LoweringDiagnosticKind::OutOfRangeIntLiteral, 56..76)],
        )
    }

    #[test]
    fn float_literal() {
        check(
            r#"
                foo :: () {
                    num := .123;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := 0.123;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn float_literal_with_underscores() {
        check(
            r#"
                foo :: () {
                    num := 1_000_000.000_00000E-3_;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := 1000;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn string_literal() {
        check(
            r#"
                foo :: () {
                    crab := "🦀";
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := "🦀";
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn string_literal_with_escapes() {
        check(
            r#"
                foo :: () {
                    escapes := "\0\a\b\n\f\r\t\v\e\'\"\\";
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := "\0\u{7}\u{8}\n\u{c}\r\t\u{b}\u{1b}'\"\\";
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn string_literal_with_invalid_escapes() {
        check(
            r#"
                foo :: () {
                    crab := "a\jb\🦀c";
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := "abc";
                };
            "#]],
            |_| {
                [
                    (LoweringDiagnosticKind::InvalidEscape, 59..61),
                    (LoweringDiagnosticKind::InvalidEscape, 62..67),
                ]
            },
        )
    }

    #[test]
    fn char_literal() {
        check(
            r#"
                foo :: () {
                    ch := 'a';
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := 'a';
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn char_literal_empty() {
        check(
            r#"
                foo :: () {
                    ch := '';
                }
            "#,
            expect![[r"
                main::foo :: () {
                    l0 := '\0';
                };
            "]],
            |_| [(LoweringDiagnosticKind::EmptyCharLiteral, 55..57)],
        )
    }

    #[test]
    fn char_literal_multiple_chars() {
        check(
            r#"
                foo :: () {
                    ch := 'Hello, World!';
                }
            "#,
            expect![[r"
                main::foo :: () {
                    l0 := '\0';
                };
            "]],
            |_| [(LoweringDiagnosticKind::TooManyCharsInCharLiteral, 55..70)],
        )
    }

    #[test]
    fn char_literal_out_of_range() {
        check(
            r#"
                foo :: () {
                    crab := '🦀';
                }
            "#,
            expect![[r"
                main::foo :: () {
                    l0 := '\0';
                };
            "]],
            |_| [(LoweringDiagnosticKind::NonU8CharLiteral, 57..63)],
        )
    }

    #[test]
    fn char_literal_with_escape() {
        check(
            r#"
                foo :: () {
                    null := '\0';
                    bell := '\a';
                    backspace := '\b';
                    linefeed := '\n';
                    formfeed := '\f';
                    carraige_return := '\r';
                    tab := '\t';
                    vertical_tab := '\v';
                    escape := '\e';
                    single_quote := '\'';
                    double_quote := '\"';
                    backslash := '\\';
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := '\0';
                    l1 := '\u{7}';
                    l2 := '\u{8}';
                    l3 := '\n';
                    l4 := '\u{c}';
                    l5 := '\r';
                    l6 := '\t';
                    l7 := '\u{b}';
                    l8 := '\u{1b}';
                    l9 := '\'';
                    l10 := '"';
                    l11 := '\\';
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn char_literal_with_invalid_escape() {
        check(
            r"
                foo :: () {
                    crab := '\🦀';
                }
            ",
            expect![[r"
                main::foo :: () {
                    l0 := '\0';
                };
            "]],
            |_| [(LoweringDiagnosticKind::InvalidEscape, 58..63)],
        )
    }

    #[test]
    fn nested_binary_expr() {
        check(
            r"
                foo :: () -> i32 {
                    1 + 2 * 3 - 4 / 5
                }
            ",
            expect![[r#"
                main::foo :: () -> i32 { 1 + 2 * 3 - 4 / 5 };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn multiple_local_defs() {
        check(
            r#"
                foo :: () {
                    a := 1;
                    b := 2;
                    c := 3;
                    d := 4;
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := 1;
                    l1 := 2;
                    l2 := 3;
                    l3 := 4;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn multiple_functions() {
        check(
            r#"
                foo :: () {}
                bar :: () {}
                baz :: () {}
                qux :: () {}
            "#,
            expect![[r#"
                main::foo :: () {};
                main::bar :: () {};
                main::baz :: () {};
                main::qux :: () {};
            "#]],
            |_| [],
        )
    }

    #[test]
    fn call_other_function() {
        check(
            r#"
                foo :: () {
                    bar()
                }

                bar :: () {
                    foo()
                }
            "#,
            expect![[r#"
                main::foo :: () { bar() };
                main::bar :: () { foo() };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn call_non_existent_function() {
        check(
            r#"
                foo :: () {
                    bar()
                }
            "#,
            expect![[r#"
                main::foo :: () { <missing>() };
            "#]],
            |i| {
                [(
                    LoweringDiagnosticKind::UndefinedRef {
                        name: i.intern("bar"),
                    },
                    49..52,
                )]
            },
        )
    }

    #[test]
    fn recursion() {
        check(
            r#"
                foo :: () {
                    foo();
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    foo();
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn lambda() {
        check(
            r#"
                foo :: () {
                    bar := () {};
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    l0 := () {};
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn lambda_dont_capture_scope() {
        check(
            r#"
                foo :: (x: i32) {
                    y := 5;

                    bar := () -> i32 {
                        x + y
                    };
                }
            "#,
            expect![[r#"
                main::foo :: (p0: i32) {
                    l0 := 5;
                    l1 := () -> i32 { <missing> + <missing> };
                };
            "#]],
            |i| {
                [
                    (
                        LoweringDiagnosticKind::UndefinedRef {
                            name: i.intern("x"),
                        },
                        127..128,
                    ),
                    (
                        LoweringDiagnosticKind::UndefinedRef {
                            name: i.intern("y"),
                        },
                        131..132,
                    ),
                ]
            },
        )
    }

    #[test]
    fn call_lambda() {
        check(
            r#"
                foo :: () -> i32 {
                    {
                        (x: i32, y: i32) -> i32 {
                            x + y
                        }
                    } (1, 2)
                }
            "#,
            expect![[r#"
                main::foo :: () -> i32 { { (p0: i32, p1: i32) -> i32 { p0 + p1 } }(1, 2) };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn extern_lambda() {
        check(
            r#"
                main :: () -> i32 {
                    puts := (s: string) extern;
                }
            "#,
            expect![[r#"
                main::main :: () -> i32 {
                    l0 := (p0: string) extern;
                };
            "#]],
            |_| [(LoweringDiagnosticKind::NonGlobalExtern, 77..83)],
        )
    }

    #[test]
    fn extern_function() {
        check(
            r#"
                puts :: (s: string) -> i32 extern;
            "#,
            expect![[r#"
                main::puts :: (p0: string) -> i32 extern;
            "#]],
            |_| [],
        )
    }

    #[test]
    fn scoped_local() {
        check(
            r#"
                foo :: () -> i32 {
                    {
                        a := 5;
                    }

                    a
                }
            "#,
            expect![[r#"
                main::foo :: () -> i32 {
                    {
                        l0 := 5;
                    };
                    <missing>
                };
            "#]],
            |i| {
                [(
                    LoweringDiagnosticKind::UndefinedRef {
                        name: i.intern("a"),
                    },
                    133..134,
                )]
            },
        )
    }

    #[test]
    fn locals_take_precedence_over_globals() {
        check(
            r#"
                bar :: () -> i32 { 0 };

                foo :: () -> i32 {
                    bar := 25;

                    bar
                }
            "#,
            expect![[r#"
                main::bar :: () -> i32 { 0 };
                main::foo :: () -> i32 {
                    l0 := 25;
                    l0
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn locals_take_precedence_over_params() {
        check(
            r#"
                main :: () -> i32 {
                    foo := {
                        bar := {
                            baz := 9;
                            baz * 10
                        };
                        bar - 1
                    };
                    foo + 3
                }
            "#,
            expect![[r#"
                main::main :: () -> i32 {
                    l2 := {
                        l1 := {
                            l0 := 9;
                            l0 * 10
                        };
                        l1 - 1
                    };
                    l2 + 3
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn array_with_inferred_size() {
        check(
            r#"
                main :: () -> i32 {
                    my_array := [] i32 { 4, 8, 15, 16, 23, 42 };
                }
            "#,
            expect![[r#"
                main::main :: () -> i32 {
                    l0 := []i32{ 4, 8, 15, 16, 23, 42 };
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn array_with_specific_size() {
        check(
            r#"
                main :: () -> i32 {
                    my_array := [6] i32 { 4, 8, 15, 16, 23, 42 };
                }
            "#,
            expect![[r#"
                main::main :: () -> i32 {
                    l0 := [6]i32{ 4, 8, 15, 16, 23, 42 };
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn array_with_incorrect_size() {
        check(
            r#"
                main :: () -> i32 {
                    my_array := [3] i32 { 4, 8, 15, 16, 23, 42 };
                }
            "#,
            expect![[r#"
                main::main :: () -> i32 {
                    l0 := [3]i32{ 4, 8, 15, 16, 23, 42 };
                };
            "#]],
            |_| {
                [(
                    LoweringDiagnosticKind::ArraySizeMismatch {
                        found: 6,
                        expected: 3,
                    },
                    77..101,
                )]
            },
        )
    }

    #[test]
    fn array_with_non_const_size() {
        check(
            r#"
                main :: () -> i32 {
                    size := 6;

                    my_array := [size] i32 { 4, 8, 15, 16, 23, 42 };
                }
            "#,
            expect![[r#"
                main::main :: () -> i32 {
                    l0 := 6;
                    l1 := []i32{ 4, 8, 15, 16, 23, 42 };
                };
            "#]],
            |_| [(LoweringDiagnosticKind::ArraySizeNotConst, 102..106)],
        )
    }

    #[test]
    fn comptime() {
        check(
            r#"
                main :: () -> i32 {
                    num :: comptime {
                        1 + 1
                    };
                }
            "#,
            expect![[r#"
                main::main :: () -> i32 {
                    l0 := comptime { 1 + 1 };
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn comptime_dont_capture_scope() {
        check(
            r#"
                main :: (x: i32) -> i32 {
                    y := 5;

                    num :: comptime {
                        x + y
                    };
                }
            "#,
            expect![[r#"
                main::main :: (p0: i32) -> i32 {
                    l0 := 5;
                    l1 := comptime { <missing> + <missing> };
                };
            "#]],
            |i| {
                [
                    (
                        LoweringDiagnosticKind::UndefinedRef {
                            name: i.intern("x"),
                        },
                        134..135,
                    ),
                    (
                        LoweringDiagnosticKind::UndefinedRef {
                            name: i.intern("y"),
                        },
                        138..139,
                    ),
                ]
            },
        )
    }

    #[test]
    fn comptime_globals() {
        check(
            r#"
                foo :: 5;

                main :: () -> i32 {
                    num :: comptime {
                        foo * 2
                    };
                }
            "#,
            expect![[r#"
                main::foo :: 5;
                main::main :: () -> i32 {
                    l0 := comptime { foo * 2 };
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn function_with_undefined_types() {
        check(
            r#"
                foo :: (x: bar, y: baz) -> qux.quux {
    
                }
            "#,
            expect![[r#"
                main::foo :: (p0: <missing>, p1: <missing>) -> <missing>.quux {};
            "#]],
            |i| {
                [
                    (
                        LoweringDiagnosticKind::UndefinedRef {
                            name: i.intern("bar"),
                        },
                        28..31,
                    ),
                    (
                        LoweringDiagnosticKind::UndefinedRef {
                            name: i.intern("baz"),
                        },
                        36..39,
                    ),
                    (
                        LoweringDiagnosticKind::UndefinedRef {
                            name: i.intern("qux"),
                        },
                        44..47,
                    ),
                ]
            },
        )
    }

    #[test]
    fn function_with_unnamed_params() {
        check(
            r#"
                foo :: (: i32, y: bool) -> i8 {
                    if y {
                        0
                    } else {
                        1
                    }
                }
            "#,
            expect![[r#"
                main::foo :: (p0: i32, p1: bool) -> i8 { if p1 { 0 } else { 1 } };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn function_with_untyped_params() {
        check(
            r#"
                foo :: (x, y) -> i8 {
                    if y {
                        0
                    } else {
                        1
                    }
                }
            "#,
            expect![[r#"
                main::foo :: (p0: <missing>, p1: <missing>) -> i8 { if p1 { 0 } else { 1 } };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn break_block() {
        check(
            r#"
                foo :: () {
                    {
                        {
                            break;
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () { { `2 {
                            break 2`;
                        } } };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn break_loop() {
        check(
            r#"
                foo :: () {
                    {
                        loop {
                            break;
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () { { `2 loop {
                            break 2`;
                        } } };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn break_block_with_label() {
        check(
            r#"
                foo :: () {
                    `blk {
                        {
                            break blk`;
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () { `1 { {
                            break 1`;
                        } } };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn break_non_existent_label() {
        check(
            r#"
                foo :: () {
                    {
                        {
                            break blk`;
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () { { {
                            break <unknown>`;
                        } } };
            "#]],
            |i| {
                [(
                    LoweringDiagnosticKind::UndefinedLabel {
                        name: i.intern("blk"),
                    },
                    111..115,
                )]
            },
        )
    }

    #[test]
    fn break_block_with_value() {
        check(
            r#"
                foo :: () -> i32 {
                    `blk {
                        {
                            break blk` 1 + 1;
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () -> i32 { `1 { {
                            break 1` 1 + 1;
                        } } };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn break_if() {
        check(
            r#"
                foo :: () -> i32 {
                    {
                        if true {
                            break true;
                        }

                        1 + 1
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () -> i32 {
                    `1 {
                        if true {
                            break 1` true;
                        };
                        1 + 1
                    }
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn continue_loop() {
        check(
            r#"
                foo :: () {
                    loop {
                        while false {
                            {
                                continue;
                            }
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    loop {
                        `2 while false { {
                                continue 2`;
                            } }
                    }
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn continue_loop_with_label() {
        check(
            r#"
                foo :: () {
                    `outer loop {
                        while false {
                            {
                                continue outer`;
                            }
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    `1 loop { while false { {
                                continue 1`;
                            } } }
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn continue_block_with_label() {
        check(
            r#"
                foo :: () {
                    `blk {
                        while false {
                            {
                                continue blk`;
                            }
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () {
                    `1 { while false { {
                                continue 1`;
                            } } }
                };
            "#]],
            |i| {
                [(
                    LoweringDiagnosticKind::ContinueNonLoop {
                        name: Some(i.intern("blk")),
                    },
                    165..169,
                )]
            },
        )
    }

    #[test]
    fn continue_block() {
        check(
            r#"
                foo :: () {
                    {
                        {
                            continue;
                        }
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () { { {
                            continue <unknown>`;
                        } } };
            "#]],
            |_| {
                [(
                    LoweringDiagnosticKind::ContinueNonLoop { name: None },
                    105..114,
                )]
            },
        )
    }

    #[test]
    fn break_function() {
        check(
            r#"
                foo :: () -> i32 {
                    break 5;
                }
            "#,
            expect![[r#"
                main::foo :: () -> i32 `0 {
                    break 0` 5;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn return_function() {
        check(
            r#"
                foo :: () -> i32 {
                    return 5;
                }
            "#,
            expect![[r#"
                main::foo :: () -> i32 `0 {
                    break 0` 5;
                };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn return_function_nested() {
        check(
            r#"
                foo :: () -> i32 {
                    {
                        return 5;
                    }
                }
            "#,
            expect![[r#"
                main::foo :: () -> i32 `0 { {
                        break 0` 5;
                    } };
            "#]],
            |_| [],
        )
    }

    #[test]
    fn return_outside_function() {
        check(
            r#"
                foo :: {
                    return 5;
                };
            "#,
            expect![[r#"
                main::foo :: `0 {
                    break 0` 5;
                };
            "#]],
            |_| [],
        )
    }
}
