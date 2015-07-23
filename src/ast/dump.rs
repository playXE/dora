use std::cell::Cell;

use ast::Ast;
use ast::Elem::{self, ElemFunction};
use ast::Expr::{self, ExprUn, ExprBin, ExprLitInt, ExprLitStr, ExprLitBool,
                ExprAssign, ExprIdent};
use ast::ExprUnType;
use ast::ExprBinType;
use ast::ExprLitIntType;
use ast::ExprLitStrType;
use ast::ExprLitBoolType;
use ast::ExprIdentType;
use ast::ExprAssignType;
use ast::Function;
use ast::Stmt::{self, StmtBlock, StmtBreak, StmtContinue, StmtExpr,
                StmtIf, StmtReturn};
use ast::StmtBlockType;
use ast::StmtBreakType;
use ast::StmtContinueType;
use ast::StmtExprType;
use ast::StmtIfType;
use ast::StmtReturnType;
use ast::Type::{self, TypeBasic, TypeUnit};
use interner::Name;

macro_rules! dump {
    ($self_:ident, $($x:expr),*) => {{
        for _ in 0..($self_.indent*2) {
            print!(" ");
        }

        println!($($x,)*);
    }};
}

pub struct AstDumper<'a> {
    ast: &'a Ast,
    indent: u32,
}

impl<'a> AstDumper<'a> {
    pub fn new(ast: &Ast) -> AstDumper {
        AstDumper {
            ast: ast,
            indent: 0
        }
    }

    pub fn dump(&mut self) {
        for el in &self.ast.elements {
            match *el {
                ElemFunction(ref fct) => self.dump_fct(fct),
                _ => unreachable!()
            }
        }
    }

    fn dump_fct(&mut self, fct: &Function) {
        dump!(self, "fct {} @ {}", self.str(fct.name), fct.pos);

        self.indent(|d| {
            if(fct.params.is_empty()) {
                dump!(d, "no params");
            } else {
                for param in &fct.params {
                    dump!(d, "param {} @ {}", d.str(param.name), param.pos);
                    d.indent(|d| d.dump_type(&param.data_type));
                }
            }
        });

        dump!(self, "fct {} returns", self.str(fct.name));
        self.indent(|d| d.dump_type(&fct.return_type));

        dump!(self, "fct {} executes", self.str(fct.name));
        self.indent(|d| d.dump_stmt(&fct.block));
    }

    fn dump_type(&mut self, ty: &Type) {
        match *ty {
            TypeBasic(name) => dump!(self, "type {}", self.str(name)),
            TypeUnit => dump!(self, "type () / void")
        }
    }

    fn dump_stmt(&mut self, stmt: &Stmt) {
        match *stmt {
            StmtBlock(ref block) => self.dump_stmt_block(block),
            StmtReturn(ref ret) => self.dump_stmt_return(ret),
            StmtBreak(ref stmt) => self.dump_stmt_break(stmt),
            StmtContinue(ref stmt) => self.dump_stmt_continue(stmt),
            StmtExpr(ref expr) => self.dump_stmt_expr(expr),
            StmtIf(ref stmt) => self.dump_stmt_if(stmt),
            _ => unimplemented!()
        }
    }

    fn dump_stmt_if(&mut self, stmt: &StmtIfType) {
        dump!(self, "if @ {}", stmt.pos);

        self.indent(|d| {
            d.indent(|d| { d.dump_expr(&stmt.cond); });
            dump!(d, "then");
            d.indent(|d| { d.dump_stmt(&stmt.then_block); });
            dump!(d, "else");
            d.indent(|d| { d.dump_stmt(&stmt.then_block); });
        });
    }

    fn dump_stmt_expr(&mut self, stmt: &StmtExprType) {
        dump!(self, "expr stmt @ {}", stmt.pos);
        self.indent(|d| { d.dump_expr(&stmt.expr); });
    }

    fn dump_stmt_block(&mut self, block: &StmtBlockType) {
        dump!(self, "block ({} statements) @ {}", block.stmts.len(), block.pos);

        self.indent(|d| {
            if(block.stmts.is_empty()) {
                dump!(d, "no statements");
            } else {
                for stmt in &block.stmts {
                    d.dump_stmt(stmt);
                }
            }
        });

        dump!(self, "block end");
    }

    fn dump_stmt_return(&mut self, ret: &StmtReturnType) {
        if let Some(ref expr) = ret.expr {
            dump!(self, "return @ {}", ret.pos);
            self.indent(|d| d.dump_expr(expr));
        } else {
            dump!(self, "return void @ {}", ret.pos);
        }
    }

    fn dump_stmt_break(&mut self, stmt: &StmtBreakType) {
        dump!(self, "break @ {}", stmt.pos);
    }

    fn dump_stmt_continue(&mut self, stmt: &StmtContinueType) {
        dump!(self, "break @ {}", stmt.pos);
    }

    fn dump_expr(&mut self, expr: &Expr) {
        match *expr {
            ExprUn(ref un) => self.dump_expr_un(un),
            ExprBin(ref bin) => self.dump_expr_bin(bin),
            ExprLitInt(ref lit) => dump!(self, "lit int {}", lit.value),
            ExprLitStr(ref lit) => dump!(self, "lit string {:?}", lit.value),
            ExprLitBool(ref lit) => dump!(self, "lit bool {}", lit.value),
            ExprIdent(ref ident) => dump!(self, "ident {}", self.str(ident.name)),
            ExprAssign(ref assign) => self.dump_expr_assign(assign),
        }
    }

    fn dump_expr_un(&mut self, expr: &ExprUnType) {
        dump!(self, "unary {:?}", expr.op);
        self.indent(|d| d.dump_expr(&expr.opnd));
    }

    fn dump_expr_bin(&mut self, expr: &ExprBinType) {
        self.indent(|d| d.dump_expr(&expr.rhs));
        dump!(self, "binary {:?}", expr.op);
        self.indent(|d| d.dump_expr(&expr.lhs));
    }

    fn dump_expr_assign(&mut self, expr: &ExprAssignType) {
        self.indent(|d| d.dump_expr(&expr.rhs));
        dump!(self, "assign (=)");
        self.indent(|d| d.dump_expr(&expr.lhs));
    }

    fn indent<F>(&mut self, fct: F) where F: Fn(&mut AstDumper) -> () {
        let old = self.indent;
        self.indent = old+1;

        fct(self);

        self.indent = old;
    }

    fn str(&self, name: Name) -> &str {
        self.ast.str(name)
    }
}
