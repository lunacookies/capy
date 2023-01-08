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

// ! This enum must match up exactly with the contents of lexer::LexerTokenKind
// ! The source of a really horrible bug
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TokenKind {
    Whitespace,
    Ident,
    Int,
    Quote,
    Escape,
    StringContents,
    Plus,
    Hyphen,
    Asterisk,
    Slash,
    Equals,
    Comma,
    Dot,
    Arrow,
    LParen,
    RParen,
    LBrace,
    RBrace,
    CommentLeader,
    CommentContents,
    Colon,
    Semicolon,
    Error
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    Root,
    Ref,
    Call,
    ArgList,
    Arg,
    Block,
    IntLiteral,
    StringLiteral,
    BinaryExpr,
    UnaryExpr,
    VarDef,
    ExprStmt,
    Lambda,
    ParamList,
    Param,
    ReturnType,
    Type,
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