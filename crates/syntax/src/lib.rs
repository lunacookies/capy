use std::mem;

pub type SyntaxBuilder = eventree::SyntaxBuilder<TreeConfig>;
pub type SyntaxElement = eventree::SyntaxElement<TreeConfig>;
pub type SyntaxNode = eventree::SyntaxNode<TreeConfig>;
pub type SyntaxToken = eventree::SyntaxToken<TreeConfig>;
pub type SyntaxTree = eventree::SyntaxTree<TreeConfig>;
pub type Event = eventree::Event<TreeConfig>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TreeConfig {}

impl eventree::TreeConfig for TreeConfig {
    type NodeKind = NodeKind;
    type TokenKind = TokenKind;
}

capy_macros::define_token_enum! {
    TokenKind, stripped, "../../tokens.lex"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    Root,
    VarRef,
    Call,
    ArgList,
    Arg,
    Array,
    ArraySize,
    ArrayBody,
    ArrayItem,
    IndexExpr, // the entire expression of indexing. e.g. `my_array[6]`
    Index,     // the actual index. `6` in `my_array[6]`
    Source,
    Distinct,
    ComptimeExpr,
    Block,
    IfExpr,
    ElseBranch,
    WhileExpr,
    Condition,
    LabelDecl,
    LabelRef,
    IntLiteral,
    FloatLiteral,
    BoolLiteral,
    CharLiteral,
    StringLiteral,
    CastExpr,
    RefExpr,
    DerefExpr,
    BinaryExpr,
    UnaryExpr,
    Binding, // `x :: 5`
    VarDef,  // `x := 5`
    Assign,
    ExprStmt,
    ReturnStmt, // todo: change these to void expressions
    BreakStmt,
    ContinueStmt,
    Lambda,
    ParamList,
    Param,
    StructDecl,    // `struct { foo: i32 }`
    FieldDecl,     // `foo: i32`
    StructLiteral, // `My_Struct { foo: 123 }`
    FieldLiteral,  // `foo: 123`
    ImportExpr,
    Ty,
    Path,
    Comment,
    Error,
}

unsafe impl eventree::SyntaxKind for TokenKind {
    fn to_raw(self) -> u16 {
        self as u16
    }

    unsafe fn from_raw(raw: u16) -> Self {
        mem::transmute(raw as u8)
    }
}

unsafe impl eventree::SyntaxKind for NodeKind {
    fn to_raw(self) -> u16 {
        self as u16
    }

    unsafe fn from_raw(raw: u16) -> Self {
        mem::transmute(raw as u8)
    }
}
