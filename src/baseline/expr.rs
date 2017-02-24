use ast::*;
use ast::Expr::*;
use baseline::codegen::{self, dump_asm, CondCode, Scopes, should_emit_asm, TempOffsets};
use baseline::fct::{CatchType, Comment};
use baseline::native;
use baseline::stub::ensure_stub;
use class::{ClassId, FieldId};
use cpu::{FReg, FREG_RESULT, FREG_TMP1, Mem, Reg, REG_RESULT, REG_TMP1, REG_TMP2, REG_PARAMS};
use ctxt::*;
use driver::cmd::AsmSyntax;
use lexer::position::Position;
use lexer::token::{FloatSuffix, IntSuffix};
use masm::*;
use mem;
use object::{Header, Str};
use os::signal::Trap;
use stdlib;
use ty::{BuiltinType, MachineMode};
use vtable::{DISPLAY_SIZE, VTable};

#[derive(Copy, Clone)]
pub enum ExprStore {
    Reg(Reg),
    FReg(FReg),
}

impl ExprStore {
    pub fn reg(&self) -> Reg {
        match self {
            &ExprStore::Reg(reg) => reg,
            _ => unreachable!(),
        }
    }

    pub fn freg(&self) -> FReg {
        match self {
            &ExprStore::FReg(reg) => reg,
            _ => unreachable!(),
        }
    }
}

impl From<Reg> for ExprStore {
    fn from(reg: Reg) -> ExprStore {
        ExprStore::Reg(reg)
    }
}

impl From<FReg> for ExprStore {
    fn from(reg: FReg) -> ExprStore {
        ExprStore::FReg(reg)
    }
}

pub struct ExprGen<'a, 'ast: 'a> {
    ctxt: &'a Context<'ast>,
    fct: &'a Fct<'ast>,
    src: &'a mut FctSrc<'ast>,
    ast: &'ast Function,
    masm: &'a mut MacroAssembler,
    scopes: &'a mut Scopes,
    tempsize: i32,
    temps: TempOffsets,
}

impl<'a, 'ast> ExprGen<'a, 'ast>
    where 'ast: 'a
{
    pub fn new(ctxt: &'a Context<'ast>,
               fct: &'a Fct<'ast>,
               src: &'a mut FctSrc<'ast>,
               ast: &'ast Function,
               masm: &'a mut MacroAssembler,
               scopes: &'a mut Scopes)
               -> ExprGen<'a, 'ast> {
        ExprGen {
            ctxt: ctxt,
            fct: fct,
            src: src,
            ast: ast,
            masm: masm,
            tempsize: 0,
            scopes: scopes,
            temps: TempOffsets::new(),
        }
    }

    pub fn generate(mut self, e: &'ast Expr, dest: ExprStore) {
        self.emit_expr(e, dest);

        if !self.temps.is_empty() {
            panic!("temporary variables are not fully freed!");
        }
    }

    fn emit_expr(&mut self, e: &'ast Expr, dest: ExprStore) {
        match *e {
            ExprLitInt(ref expr) => self.emit_lit_int(expr, dest.reg()),
            ExprLitFloat(ref expr) => self.emit_lit_float(expr, dest.freg()),
            ExprLitBool(ref expr) => self.emit_lit_bool(expr, dest.reg()),
            ExprLitStr(ref expr) => self.emit_lit_str(expr, dest.reg()),
            ExprLitStruct(_) => unimplemented!(),
            ExprUn(ref expr) => self.emit_un(expr, dest),
            ExprIdent(ref expr) => self.emit_ident(expr, dest),
            ExprAssign(ref expr) => self.emit_assign(expr, dest),
            ExprBin(ref expr) => self.emit_bin(expr, dest),
            ExprCall(ref expr) => self.emit_call(expr, dest),
            ExprDelegation(ref expr) => self.emit_delegation(expr, dest),
            ExprField(ref expr) => self.emit_field(expr, dest),
            ExprSelf(_) => self.emit_self(dest.reg()),
            ExprSuper(_) => self.emit_self(dest.reg()),
            ExprNil(_) => self.emit_nil(dest.reg()),
            ExprArray(ref expr) => self.emit_array(expr, dest),
            ExprConv(ref expr) => self.emit_conv(expr, dest.reg()),
            ExprTry(ref expr) => self.emit_try(expr, dest),
        }
    }

    fn emit_try(&mut self, e: &'ast ExprTryType, dest: ExprStore) {
        match e.mode {
            TryMode::Normal => {
                self.emit_expr(&e.expr, dest);
            }

            TryMode::Else(ref alt_expr) => {
                let lbl_after = self.masm.create_label();

                let try_span = {
                    let start = self.masm.pos();
                    self.emit_expr(&e.expr, dest);
                    let end = self.masm.pos();

                    self.masm.jump(lbl_after);

                    (start, end)
                };

                let catch_span = {
                    let start = self.masm.pos();
                    self.emit_expr(alt_expr, dest);
                    let end = self.masm.pos();

                    (start, end)
                };

                self.masm.emit_exception_handler(try_span, catch_span.0, None, CatchType::Any);
                self.masm.bind_label(lbl_after);
            }

            TryMode::Force => {
                let lbl_after = self.masm.create_label();

                let try_span = {
                    let start = self.masm.pos();
                    self.emit_expr(&e.expr, dest);
                    let end = self.masm.pos();

                    self.masm.jump(lbl_after);

                    (start, end)
                };

                let catch_span = {
                    let start = self.masm.pos();
                    self.masm.emit_bailout_inplace(Trap::UNEXPECTED, e.pos);
                    let end = self.masm.pos();

                    (start, end)
                };

                self.masm.emit_exception_handler(try_span, catch_span.0, None, CatchType::Any);
                self.masm.bind_label(lbl_after);
            }

            TryMode::Opt => panic!("unsupported"),
        }
    }

    fn emit_conv(&mut self, e: &'ast ExprConvType, dest: Reg) {
        self.emit_expr(&e.object, dest.into());

        // return false if object is nil
        let lbl_nil = self.masm.test_if_nil(dest);
        let conv = *self.src.map_convs.get(e.id).unwrap();

        if conv.valid {
            if e.is {
                // return true for object is T
                self.masm.load_true(dest);

            } else {
                // do nothing for object as T
            }

        } else {
            let cls_id = conv.cls_id;
            let cls = self.ctxt.classes[cls_id].borrow();
            let vtable: &VTable = cls.vtable.as_ref().unwrap();

            let offset = if e.is {
                0
            } else {
                // reserve temp variable for object
                let offset = self.reserve_temp_for_node(&e.object);
                self.masm.store_mem(MachineMode::Ptr, Mem::Local(offset), dest);

                offset
            };

            // object instanceof T

            // tmp1 = <vtable of object>
            self.masm.load_mem(MachineMode::Ptr, REG_TMP1, Mem::Base(dest, 0));

            let disp = self.masm.add_addr(vtable as *const _ as *mut u8);
            let pos = self.masm.pos() as i32;

            // tmp2 = <vtable of T>
            self.masm.load_constpool(REG_TMP2, disp + pos);

            if vtable.subtype_depth >= DISPLAY_SIZE as i32 {
                // cmp [tmp1 + offset T.vtable.subtype_depth], tmp3
                self.masm.cmp_mem_imm(MachineMode::Int32,
                                      Mem::Base(REG_TMP1, VTable::offset_of_depth()),
                                      vtable.subtype_depth);

                // jnz lbl_false
                let lbl_false = self.masm.create_label();
                self.masm.jump_if(CondCode::Less, lbl_false);

                // tmp1 = tmp1.subtype_overflow
                self.masm.load_mem(MachineMode::Ptr,
                                   REG_TMP1,
                                   Mem::Base(REG_TMP1, VTable::offset_of_overflow()));

                let overflow_offset = mem::ptr_width() *
                                      (vtable.subtype_depth - DISPLAY_SIZE as i32);

                // cmp [tmp1 + 8*(vtable.subtype_depth - DISPLAY_SIZE) ], tmp2
                self.masm.cmp_mem(MachineMode::Ptr,
                                  Mem::Base(REG_TMP1, overflow_offset),
                                  REG_TMP2);

                if e.is {
                    // dest = if zero then true else false
                    self.masm.set(dest, CondCode::Equal);

                } else {
                    // jump to lbl_false if cmp did not succeed
                    self.masm.jump_if(CondCode::NonZero, lbl_false);

                    // otherwise load temp variable again
                    self.masm.load_mem(MachineMode::Ptr, dest, Mem::Local(offset));
                }

                // jmp lbl_finished
                let lbl_finished = self.masm.create_label();
                self.masm.jump(lbl_finished);

                // lbl_false:
                self.masm.bind_label(lbl_false);

                if e.is {
                    // dest = false
                    self.masm.load_false(dest);
                } else {
                    // bailout
                    self.masm.emit_bailout_inplace(Trap::CAST, e.pos);
                }

                // lbl_finished:
                self.masm.bind_label(lbl_finished);
            } else {
                let display_entry = VTable::offset_of_display() +
                                    vtable.subtype_depth * mem::ptr_width();

                // tmp1 = vtable of object
                // tmp2 = vtable of T
                // cmp [tmp1 + offset], tmp2
                self.masm.cmp_mem(MachineMode::Ptr,
                                  Mem::Base(REG_TMP1, display_entry),
                                  REG_TMP2);

                if e.is {
                    self.masm.set(dest, CondCode::Equal);

                } else {
                    let lbl_bailout = self.masm.create_label();
                    self.masm.jump_if(CondCode::NotEqual, lbl_bailout);
                    self.masm.emit_bailout(lbl_bailout, Trap::CAST, e.pos);

                    self.masm.load_mem(MachineMode::Ptr, dest, Mem::Local(offset));
                }
            }

            if !e.is {
                self.free_temp_for_node(&e.object, offset);
            }
        }

        // lbl_nil:
        self.masm.bind_label(lbl_nil);

        // for is we are finished: dest is null which is boolean false
        // also for as we are finished: dest is null and stays null
    }

    fn emit_array(&mut self, e: &'ast ExprArrayType, dest: ExprStore) {
        if let Some(intrinsic) = self.intrinsic(e.id) {
            match intrinsic {
                Intrinsic::LongArrayGet => {
                    self.emit_array_get(e.pos, MachineMode::Int64, &e.object, &e.index, dest.reg())
                }

                Intrinsic::IntArrayGet => {
                    self.emit_array_get(e.pos, MachineMode::Int32, &e.object, &e.index, dest.reg())
                }

                Intrinsic::ByteArrayGet | Intrinsic::StrGet => {
                    self.emit_array_get(e.pos, MachineMode::Int8, &e.object, &e.index, dest.reg())
                }

                _ => panic!("unexpected intrinsic {:?}", intrinsic),
            }

        } else {
            self.emit_universal_call(e.id, e.pos, dest);
        }
    }

    fn reserve_temp_for_node(&mut self, expr: &Expr) -> i32 {
        let id = expr.id();
        let ty = self.src.ty(id);
        let offset = -(self.src.localsize + self.src.get_store(id).offset());

        if ty.reference_type() {
            self.temps.insert(offset);
        }

        offset
    }

    fn reserve_temp_for_arg(&mut self, arg: &Arg<'ast>) -> i32 {
        let offset = -(self.src.localsize + arg.offset());
        let ty = arg.ty();

        if ty.reference_type() {
            self.temps.insert(offset);
        }

        offset
    }

    fn free_temp_for_node(&mut self, expr: &Expr, offset: i32) {
        let ty = self.src.ty(expr.id());

        if ty.reference_type() {
            self.temps.remove(offset);
        }
    }

    fn free_temp_with_type(&mut self, ty: BuiltinType, offset: i32) {
        if ty.reference_type() {
            self.temps.remove(offset);
        }
    }

    fn intrinsic(&self, id: NodeId) -> Option<Intrinsic> {
        let call = self.src.map_calls.get(id);
        if call.is_none() {
            return None;
        }

        let fid = call.unwrap().fct_id();

        // the function we compile right now is never an intrinsic
        if self.fct.id == fid {
            return None;
        }

        let fct = self.ctxt.fcts[fid].borrow();

        match fct.kind {
            FctKind::Builtin(intrinsic) => Some(intrinsic),
            _ => None,
        }
    }

    fn emit_self(&mut self, dest: Reg) {
        let var = self.src.var_self();

        self.masm.emit_comment(Comment::LoadSelf(var.id));
        self.masm.load_mem(var.ty.mode(), dest, Mem::Local(var.offset));
    }

    fn emit_nil(&mut self, dest: Reg) {
        self.masm.load_nil(dest);
    }

    fn emit_field(&mut self, expr: &'ast ExprFieldType, dest: ExprStore) {
        let (cls, field) = {
            let ident_type = self.src.map_idents.get(expr.id).unwrap();

            match ident_type {
                &IdentType::Field(cls, field) => (cls, field),
                _ => unreachable!(),
            }
        };

        self.emit_expr(&expr.object, REG_RESULT.into());
        self.emit_field_access(cls, field, REG_RESULT, dest);
    }

    fn emit_field_access(&mut self, clsid: ClassId, fieldid: FieldId, src: Reg, dest: ExprStore) {
        let cls = self.ctxt.classes[clsid].borrow();
        let field = &cls.fields[fieldid];

        self.masm.emit_comment(Comment::LoadField(clsid, fieldid));
        self.masm.load_mem(field.ty.mode(), dest.reg(), Mem::Base(src, field.offset));
    }

    fn emit_lit_int(&mut self, lit: &'ast ExprLitIntType, dest: Reg) {
        let ty = match lit.suffix {
            IntSuffix::Byte => MachineMode::Int8,
            IntSuffix::Int => MachineMode::Int32,
            IntSuffix::Long => MachineMode::Int64,
        };

        self.masm.load_int_const(ty, dest, lit.value as i64);
    }

    fn emit_lit_float(&mut self, lit: &'ast ExprLitFloatType, dest: FReg) {
        let ty = match lit.suffix {
            FloatSuffix::Float => MachineMode::Float32,
            FloatSuffix::Double => MachineMode::Float64,
        };

        self.masm.load_float_const(ty, dest, lit.value);
    }

    fn emit_lit_bool(&mut self, lit: &'ast ExprLitBoolType, dest: Reg) {
        if lit.value {
            self.masm.load_true(dest);
        } else {
            self.masm.load_false(dest);
        };
    }

    fn emit_lit_str(&mut self, lit: &'ast ExprLitStrType, dest: Reg) {
        let handle = Str::from_buffer_in_perm(self.ctxt, lit.value.as_bytes());

        let disp = self.masm.add_addr(handle.raw() as *const u8);
        let pos = self.masm.pos() as i32;

        self.masm.emit_comment(Comment::LoadString(handle));
        self.masm.load_constpool(dest, disp + pos);
    }

    fn emit_ident(&mut self, e: &'ast ExprIdentType, dest: ExprStore) {
        let &ident = self.src.map_idents.get(e.id).unwrap();

        match ident {
            IdentType::Var(varid) => {
                self.masm.emit_comment(Comment::LoadVar(varid));
                codegen::var_load(self.masm, self.src, varid, dest.reg())
            }

            IdentType::Field(cls, field) => {
                self.emit_self(REG_RESULT);
                self.emit_field_access(cls, field, REG_RESULT, dest);
            }

            IdentType::Struct(_) => {
                unimplemented!();
            }
        }
    }

    fn emit_un(&mut self, e: &'ast ExprUnType, dest: ExprStore) {
        self.emit_expr(&e.opnd, dest);

        if let Some(intrinsic) = self.intrinsic(e.id) {
            match intrinsic {
                Intrinsic::IntPlus | Intrinsic::LongPlus => {}

                Intrinsic::IntNeg | Intrinsic::LongNeg => {
                    let dest = dest.reg();

                    let mode = if intrinsic == Intrinsic::IntNeg {
                        MachineMode::Int32
                    } else {
                        MachineMode::Int64
                    };

                    self.masm.int_neg(mode, dest, dest);
                }

                Intrinsic::ByteNot => {
                    let dest = dest.reg();
                    self.masm.int_not(MachineMode::Int8, dest, dest)
                }

                Intrinsic::IntNot | Intrinsic::LongNot => {
                    let dest = dest.reg();

                    let mode = if intrinsic == Intrinsic::IntNot {
                        MachineMode::Int32
                    } else {
                        MachineMode::Int64
                    };

                    self.masm.int_not(mode, dest, dest);
                }

                Intrinsic::BoolNot => {
                    let dest = dest.reg();
                    self.masm.bool_not(dest, dest)
                }

                _ => panic!("unexpected intrinsic {:?}", intrinsic),
            }

        } else {
            self.emit_universal_call(e.id, e.pos, dest);

        }
    }

    fn emit_assign(&mut self, e: &'ast ExprAssignType, dest: ExprStore) {
        if e.lhs.is_array() {
            let array = e.lhs.to_array().unwrap();

            if let Some(intrinsic) = self.intrinsic(e.id) {
                match intrinsic {
                    Intrinsic::ByteArraySet | Intrinsic::StrSet => {
                        self.emit_array_set(e.pos,
                                            MachineMode::Int8,
                                            &array.object,
                                            &array.index,
                                            &e.rhs,
                                            dest.reg())
                    }

                    Intrinsic::IntArraySet => {
                        self.emit_array_set(e.pos,
                                            MachineMode::Int32,
                                            &array.object,
                                            &array.index,
                                            &e.rhs,
                                            dest.reg())
                    }

                    Intrinsic::LongArraySet => {
                        self.emit_array_set(e.pos,
                                            MachineMode::Int64,
                                            &array.object,
                                            &array.index,
                                            &e.rhs,
                                            dest.reg())
                    }

                    _ => panic!("unexpected intrinsic {:?}", intrinsic),
                }

            } else {
                self.emit_universal_call(e.id, e.pos, dest);
            }

            return;
        }

        let &ident_type = self.src.map_idents.get(e.lhs.id()).unwrap();

        match ident_type {
            IdentType::Var(varid) => {
                self.emit_expr(&e.rhs, dest);

                self.masm.emit_comment(Comment::StoreVar(varid));
                codegen::var_store(&mut self.masm, self.src, dest.reg(), varid);
            }

            IdentType::Field(clsid, fieldid) => {
                let cls = self.ctxt.classes[clsid].borrow();
                let field = &cls.fields[fieldid];

                let temp = if let Some(expr_field) = e.lhs.to_field() {
                    self.emit_expr(&expr_field.object, REG_RESULT.into());

                    &expr_field.object

                } else {
                    self.emit_self(REG_RESULT);

                    &e.lhs
                };

                let temp_offset = self.reserve_temp_for_node(temp);
                self.masm.store_mem(MachineMode::Ptr, Mem::Local(temp_offset), REG_RESULT);

                self.emit_expr(&e.rhs, REG_RESULT.into());
                self.masm.load_mem(MachineMode::Ptr, REG_TMP1, Mem::Local(temp_offset));

                self.masm.emit_comment(Comment::StoreField(clsid, fieldid));
                self.masm.store_mem(field.ty.mode(),
                                    Mem::Base(REG_TMP1, field.offset),
                                    REG_RESULT);
                self.free_temp_for_node(temp, temp_offset);

                if REG_RESULT != dest.reg() {
                    self.masm.copy_reg(field.ty.mode(), dest.reg(), REG_RESULT);
                }
            }

            IdentType::Struct(_) => {
                unimplemented!();
            }
        }
    }

    fn emit_bin(&mut self, e: &'ast ExprBinType, dest: ExprStore) {
        if let Some(intrinsic) = self.intrinsic(e.id) {
            self.emit_intrinsic_bin(&e.lhs, &e.rhs, dest, intrinsic, Some(e.op));

        } else if e.op == BinOp::Cmp(CmpOp::Is) || e.op == BinOp::Cmp(CmpOp::IsNot) {
            self.emit_expr(&e.lhs, REG_RESULT.into());
            let offset = self.reserve_temp_for_node(&e.lhs);
            self.masm.store_mem(MachineMode::Ptr, Mem::Local(offset), REG_RESULT);

            self.emit_expr(&e.rhs, REG_TMP1.into());
            self.masm.load_mem(MachineMode::Ptr, REG_RESULT, Mem::Local(offset));

            self.masm.cmp_reg(MachineMode::Ptr, REG_RESULT, REG_TMP1);

            let op = match e.op {
                BinOp::Cmp(CmpOp::Is) => CondCode::Equal,
                _ => CondCode::NotEqual,
            };

            self.masm.set(dest.reg(), op);
            self.free_temp_for_node(&e.lhs, offset);

        } else if e.op == BinOp::Or {
            self.emit_bin_or(e, dest.reg());

        } else if e.op == BinOp::And {
            self.emit_bin_and(e, dest.reg());

        } else {
            self.emit_universal_call(e.id, e.pos, dest);

            match e.op {
                BinOp::Cmp(CmpOp::Eq) => {}
                BinOp::Cmp(CmpOp::Ne) => {
                    let dest = dest.reg();
                    self.masm.bool_not(dest, dest);
                }

                BinOp::Cmp(op) => {
                    let dest = dest.reg();

                    let temp = if dest == REG_RESULT {
                        REG_TMP1
                    } else {
                        REG_RESULT
                    };

                    self.masm.load_int_const(MachineMode::Int32, temp, 0);
                    self.masm.cmp_reg(MachineMode::Int32, dest, temp);
                    self.masm.set(dest, to_cond_code(op));
                }
                _ => {}
            }
        }
    }

    fn emit_bin_or(&mut self, e: &'ast ExprBinType, dest: Reg) {
        let lbl_true = self.masm.create_label();
        let lbl_false = self.masm.create_label();
        let lbl_end = self.masm.create_label();

        self.emit_expr(&e.lhs, REG_RESULT.into());
        self.masm.test_and_jump_if(CondCode::NonZero, REG_RESULT, lbl_true);

        self.emit_expr(&e.rhs, REG_RESULT.into());
        self.masm.test_and_jump_if(CondCode::Zero, REG_RESULT, lbl_false);

        self.masm.bind_label(lbl_true);
        self.masm.load_true(dest);
        self.masm.jump(lbl_end);

        self.masm.bind_label(lbl_false);
        self.masm.load_false(dest);

        self.masm.bind_label(lbl_end);
    }

    fn emit_bin_and(&mut self, e: &'ast ExprBinType, dest: Reg) {
        let lbl_true = self.masm.create_label();
        let lbl_false = self.masm.create_label();
        let lbl_end = self.masm.create_label();

        self.emit_expr(&e.lhs, REG_RESULT.into());
        self.masm.test_and_jump_if(CondCode::Zero, REG_RESULT, lbl_false);

        self.emit_expr(&e.rhs, REG_RESULT.into());
        self.masm.test_and_jump_if(CondCode::Zero, REG_RESULT, lbl_false);

        self.masm.bind_label(lbl_true);
        self.masm.load_true(dest);
        self.masm.jump(lbl_end);

        self.masm.bind_label(lbl_false);
        self.masm.load_false(dest);

        self.masm.bind_label(lbl_end);
    }

    fn ptr_for_fct_id(&mut self, fid: FctId) -> *const u8 {
        if self.fct.id == fid {
            // we want to recursively invoke the function we are compiling right now
            ensure_jit_or_stub_ptr(self.src, self.ctxt)

        } else {
            let fct = self.ctxt.fcts[fid].borrow();

            match fct.kind {
                FctKind::Source(_) => {
                    let src = fct.src();
                    let mut src = src.lock().unwrap();

                    ensure_jit_or_stub_ptr(&mut src, self.ctxt)
                }

                FctKind::Native(ptr) => {
                    ensure_native_stub(self.ctxt, fid, ptr, fct.return_type, fct.real_args())
                }

                FctKind::Definition => unreachable!(),
                FctKind::Builtin(_) => panic!("intrinsic fct call"),
            }
        }
    }

    fn emit_call(&mut self, e: &'ast ExprCallType, dest: ExprStore) {
        if let Some(intrinsic) = self.intrinsic(e.id) {
            match intrinsic {
                Intrinsic::ByteArrayLen | Intrinsic::IntArrayLen | Intrinsic::LongArrayLen => {
                    self.emit_intrinsic_len(e, dest.reg())
                }
                Intrinsic::Assert => self.emit_intrinsic_assert(e, dest.reg()),
                Intrinsic::Shl => self.emit_intrinsic_shl(e, dest.reg()),
                Intrinsic::SetUint8 => self.emit_set_uint8(e, dest.reg()),
                Intrinsic::StrLen => self.emit_intrinsic_len(e, dest.reg()),
                Intrinsic::StrGet => {
                    self.emit_array_get(e.pos,
                                        MachineMode::Int8,
                                        e.object.as_ref().unwrap(),
                                        &e.args[0],
                                        dest.reg())
                }

                Intrinsic::BoolToInt | Intrinsic::ByteToInt => {
                    self.emit_intrinsic_byte_to_int(e, dest.reg())
                }
                Intrinsic::BoolToLong | Intrinsic::ByteToLong => {
                    self.emit_intrinsic_byte_to_long(e, dest.reg())
                }
                Intrinsic::LongToByte => self.emit_intrinsic_long_to_byte(e, dest.reg()),
                Intrinsic::LongToInt => self.emit_intrinsic_long_to_int(e, dest.reg()),
                Intrinsic::IntToByte => self.emit_intrinsic_int_to_byte(e, dest.reg()),
                Intrinsic::IntToLong => self.emit_intrinsic_int_to_long(e, dest.reg()),

                Intrinsic::ByteEq => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::ByteCmp => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::ByteNot => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::BoolEq => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::BoolNot => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::IntEq => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntCmp => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::IntAdd => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntSub => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntMul => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntDiv => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntMod => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::IntOr => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntAnd => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntXor => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::IntShl => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntSar => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::IntShr => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::LongEq => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongCmp => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::LongAdd => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongSub => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongMul => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongDiv => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongMod => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::LongOr => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongAnd => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongXor => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::LongShl => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongSar => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::LongShr => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::FloatAdd => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::FloatSub => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::FloatMul => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::FloatDiv => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                Intrinsic::DoubleAdd => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::DoubleSub => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::DoubleMul => self.emit_intrinsic_bin_call(e, dest, intrinsic),
                Intrinsic::DoubleDiv => self.emit_intrinsic_bin_call(e, dest, intrinsic),

                _ => panic!("unknown intrinsic {:?}", intrinsic),
            }
        } else {
            self.emit_universal_call(e.id, e.pos, dest);
        }
    }

    fn emit_array_set(&mut self,
                      pos: Position,
                      mode: MachineMode,
                      object: &'ast Expr,
                      index: &'ast Expr,
                      rhs: &'ast Expr,
                      dest: Reg) {
        self.emit_expr(object, REG_RESULT.into());
        let offset_object = self.reserve_temp_for_node(object);
        self.masm.store_mem(MachineMode::Ptr, Mem::Local(offset_object), REG_RESULT);

        self.emit_expr(index, REG_RESULT.into());
        let offset_index = self.reserve_temp_for_node(index);
        self.masm.store_mem(MachineMode::Int32, Mem::Local(offset_index), REG_RESULT);

        self.emit_expr(rhs, REG_RESULT.into());
        let offset_value = self.reserve_temp_for_node(rhs);
        self.masm.store_mem(mode, Mem::Local(offset_value), REG_RESULT);

        self.masm.load_mem(MachineMode::Ptr, REG_TMP1, Mem::Local(offset_object));
        self.masm.load_mem(MachineMode::Int32, REG_TMP2, Mem::Local(offset_index));

        if !self.ctxt.args.flag_omit_bounds_check {
            self.masm.check_index_out_of_bounds(pos, REG_TMP1, REG_TMP2, REG_RESULT);
        }

        self.masm.load_mem(mode, REG_RESULT, Mem::Local(offset_value));
        self.masm.store_array_elem(mode, REG_TMP1, REG_TMP2, REG_RESULT);

        self.free_temp_for_node(object, offset_object);
        self.free_temp_for_node(index, offset_index);
        self.free_temp_for_node(rhs, offset_value);

        if dest != REG_RESULT {
            self.masm.copy_reg(mode, dest, REG_RESULT);
        }
    }

    fn emit_array_get(&mut self,
                      pos: Position,
                      mode: MachineMode,
                      object: &'ast Expr,
                      index: &'ast Expr,
                      dest: Reg) {
        self.emit_expr(object, REG_RESULT.into());
        let offset = self.reserve_temp_for_node(object);
        self.masm.store_mem(MachineMode::Ptr, Mem::Local(offset), REG_RESULT);

        self.emit_expr(index, REG_TMP1.into());
        self.masm.load_mem(MachineMode::Ptr, REG_RESULT, Mem::Local(offset));

        if !self.ctxt.args.flag_omit_bounds_check {
            self.masm.check_index_out_of_bounds(pos, REG_RESULT, REG_TMP1, REG_TMP2);
        }

        self.masm.load_array_elem(mode, REG_RESULT, REG_RESULT, REG_TMP1);

        self.free_temp_for_node(object, offset);

        if dest != REG_RESULT {
            self.masm.copy_reg(mode, dest, REG_RESULT);
        }
    }

    fn emit_set_uint8(&mut self, e: &'ast ExprCallType, _: Reg) {
        self.emit_expr(&e.args[0], REG_RESULT.into());
        let offset = self.reserve_temp_for_node(&e.args[0]);
        self.masm.store_mem(MachineMode::Int64, Mem::Local(offset), REG_RESULT);

        self.emit_expr(&e.args[1], REG_TMP1.into());
        self.masm.load_mem(MachineMode::Int64, REG_RESULT, Mem::Local(offset));

        self.masm.store_mem(MachineMode::Int8, Mem::Base(REG_RESULT, 0), REG_TMP1);
    }

    fn emit_intrinsic_len(&mut self, e: &'ast ExprCallType, dest: Reg) {
        self.emit_expr(&e.object.as_ref().unwrap(), REG_RESULT.into());
        self.masm.test_if_nil_bailout(e.pos, REG_RESULT, Trap::NIL);
        self.masm.load_mem(MachineMode::Ptr,
                           dest,
                           Mem::Base(REG_RESULT, Header::size()));
    }

    fn emit_intrinsic_assert(&mut self, e: &'ast ExprCallType, _: Reg) {
        let lbl_div = self.masm.create_label();
        self.emit_expr(&e.args[0], REG_RESULT.into());

        self.masm.emit_comment(Comment::Lit("check assert"));
        self.masm.test_and_jump_if(CondCode::Zero, REG_RESULT, lbl_div);
        self.masm.emit_bailout(lbl_div, Trap::ASSERT, e.pos);
    }

    fn emit_intrinsic_shl(&mut self, e: &'ast ExprCallType, dest: Reg) {
        self.emit_expr(&e.args[0], REG_RESULT.into());
        let offset = self.reserve_temp_for_node(&e.args[0]);
        self.masm.store_mem(MachineMode::Int32, Mem::Local(offset), REG_RESULT);

        self.emit_expr(&e.args[1], REG_TMP1.into());
        self.masm.load_mem(MachineMode::Int32, REG_RESULT, Mem::Local(offset));

        self.masm.int_shl(MachineMode::Int32, dest, REG_RESULT, REG_TMP1);
    }

    fn emit_intrinsic_long_to_int(&mut self, e: &'ast ExprCallType, dest: Reg) {
        self.emit_expr(e.object.as_ref().unwrap(), dest.into());
    }

    fn emit_intrinsic_long_to_byte(&mut self, e: &'ast ExprCallType, dest: Reg) {
        self.emit_expr(e.object.as_ref().unwrap(), dest.into());
        self.masm.extend_byte(MachineMode::Int32, dest, dest);
    }

    fn emit_intrinsic_int_to_byte(&mut self, e: &'ast ExprCallType, dest: Reg) {
        self.emit_expr(e.object.as_ref().unwrap(), dest.into());
        self.masm.extend_byte(MachineMode::Int32, dest, dest);
    }

    fn emit_intrinsic_int_to_long(&mut self, e: &'ast ExprCallType, dest: Reg) {
        self.emit_expr(e.object.as_ref().unwrap(), REG_RESULT.into());
        self.masm.extend_int_long(dest, REG_RESULT);
    }

    fn emit_intrinsic_byte_to_int(&mut self, e: &'ast ExprCallType, dest: Reg) {
        self.emit_expr(e.object.as_ref().unwrap(), dest.into());
        self.masm.extend_byte(MachineMode::Int32, dest, dest);
    }

    fn emit_intrinsic_byte_to_long(&mut self, e: &'ast ExprCallType, dest: Reg) {
        self.emit_expr(e.object.as_ref().unwrap(), dest.into());
        self.masm.extend_byte(MachineMode::Int64, dest, dest);
    }

    fn emit_intrinsic_bin_call(&mut self,
                               e: &'ast ExprCallType,
                               dest: ExprStore,
                               intr: Intrinsic) {
        let lhs = e.object.as_ref().unwrap();
        let rhs = &e.args[0];

        self.emit_intrinsic_bin(lhs, rhs, dest, intr, None);
    }

    fn emit_intrinsic_bin(&mut self,
                          lhs: &'ast Expr,
                          rhs: &'ast Expr,
                          dest: ExprStore,
                          intr: Intrinsic,
                          op: Option<BinOp>) {
        let mode = self.src.ty(lhs.id()).mode();

        let (lhs_reg, rhs_reg) = if mode.is_float() {
            (FREG_RESULT.into(), FREG_TMP1.into())
        } else {
            (REG_RESULT.into(), REG_TMP1.into())
        };

        self.emit_expr(lhs, lhs_reg);
        let offset = self.reserve_temp_for_node(lhs);

        if mode.is_float() {
            self.masm.storef_mem(mode, Mem::Local(offset), lhs_reg.freg());
        } else {
            self.masm.store_mem(mode, Mem::Local(offset), lhs_reg.reg());
        }

        self.emit_expr(rhs, rhs_reg);

        if mode.is_float() {
            self.masm.loadf_mem(mode, lhs_reg.freg(), Mem::Local(offset));
        } else {
            self.masm.load_mem(mode, lhs_reg.reg(), Mem::Local(offset));
        }

        if mode.is_float() {
            let lhs_reg = lhs_reg.freg();
            let rhs_reg = rhs_reg.freg();

            self.emit_intrinsic_float(dest, lhs_reg, rhs_reg, intr, op);
        } else {
            let lhs_reg = lhs_reg.reg();
            let rhs_reg = rhs_reg.reg();

            self.emit_intrinsic_int(dest.reg(), lhs_reg, rhs_reg, intr, op);
        }
    }

    fn emit_intrinsic_int(&mut self,
                          dest: Reg,
                          lhs: Reg,
                          rhs: Reg,
                          intr: Intrinsic,
                          op: Option<BinOp>) {
        match intr {
            Intrinsic::ByteEq | Intrinsic::BoolEq | Intrinsic::IntEq | Intrinsic::LongEq => {
                let mode = if intr == Intrinsic::LongEq {
                    MachineMode::Int64
                } else {
                    MachineMode::Int32
                };

                let cond_code = match op {
                    Some(BinOp::Cmp(cmp)) => to_cond_code(cmp),
                    _ => CondCode::Equal,
                };

                self.masm.cmp_reg(mode, lhs, rhs);
                self.masm.set(dest, cond_code);
            }

            Intrinsic::ByteCmp | Intrinsic::IntCmp | Intrinsic::LongCmp => {
                let mode = if intr == Intrinsic::LongCmp {
                    MachineMode::Int64
                } else {
                    MachineMode::Int32
                };

                if let Some(BinOp::Cmp(op)) = op {
                    let cond_code = to_cond_code(op);

                    self.masm.cmp_reg(mode, lhs, rhs);
                    self.masm.set(dest, cond_code);
                } else {
                    self.masm.int_sub(mode, dest, lhs, rhs);
                }
            }

            Intrinsic::IntAdd => self.masm.int_add(MachineMode::Int32, dest, lhs, rhs),
            Intrinsic::IntSub => self.masm.int_sub(MachineMode::Int32, dest, lhs, rhs),
            Intrinsic::IntMul => self.masm.int_mul(MachineMode::Int32, dest, lhs, rhs),
            Intrinsic::IntDiv => self.masm.int_div(MachineMode::Int32, dest, lhs, rhs),
            Intrinsic::IntMod => self.masm.int_mod(MachineMode::Int32, dest, lhs, rhs),

            Intrinsic::IntOr => self.masm.int_or(MachineMode::Int32, dest, lhs, rhs),
            Intrinsic::IntAnd => self.masm.int_and(MachineMode::Int32, dest, lhs, rhs),
            Intrinsic::IntXor => self.masm.int_xor(MachineMode::Int32, dest, lhs, rhs),

            Intrinsic::IntShl => self.masm.int_shl(MachineMode::Int32, dest, lhs, rhs),
            Intrinsic::IntSar => self.masm.int_sar(MachineMode::Int32, dest, lhs, rhs),
            Intrinsic::IntShr => self.masm.int_shr(MachineMode::Int32, dest, lhs, rhs),

            Intrinsic::LongAdd => self.masm.int_add(MachineMode::Int64, dest, lhs, rhs),
            Intrinsic::LongSub => self.masm.int_sub(MachineMode::Int64, dest, lhs, rhs),
            Intrinsic::LongMul => self.masm.int_mul(MachineMode::Int64, dest, lhs, rhs),
            Intrinsic::LongDiv => self.masm.int_div(MachineMode::Int64, dest, lhs, rhs),
            Intrinsic::LongMod => self.masm.int_mod(MachineMode::Int64, dest, lhs, rhs),

            Intrinsic::LongOr => self.masm.int_or(MachineMode::Int64, dest, lhs, rhs),
            Intrinsic::LongAnd => self.masm.int_and(MachineMode::Int64, dest, lhs, rhs),
            Intrinsic::LongXor => self.masm.int_xor(MachineMode::Int64, dest, lhs, rhs),

            Intrinsic::LongShl => self.masm.int_shl(MachineMode::Int64, dest, lhs, rhs),
            Intrinsic::LongSar => self.masm.int_sar(MachineMode::Int64, dest, lhs, rhs),
            Intrinsic::LongShr => self.masm.int_shr(MachineMode::Int64, dest, lhs, rhs),

            _ => panic!("unexpected intrinsic {:?}", intr),
        }
    }

    fn emit_intrinsic_float(&mut self,
                            dest: ExprStore,
                            lhs: FReg,
                            rhs: FReg,
                            intr: Intrinsic,
                            op: Option<BinOp>) {
        use ty::MachineMode::{Float32, Float64};

        match intr {
            Intrinsic::FloatEq | Intrinsic::DoubleEq => {
                let mode = if intr == Intrinsic::DoubleEq {
                    Float64
                } else {
                    Float32
                };

                let cond_code = match op {
                    Some(BinOp::Cmp(cmp)) => to_cond_code(cmp),
                    _ => CondCode::Equal,
                };

                self.masm.cmp_freg(mode, lhs, rhs);
                self.masm.set(dest.reg(), cond_code);
            }

            Intrinsic::FloatCmp | Intrinsic::DoubleCmp => {
                let mode = if intr == Intrinsic::DoubleCmp {
                    Float64
                } else {
                    Float32
                };

                if let Some(BinOp::Cmp(op)) = op {
                    let cond_code = to_cond_code(op);

                    self.masm.cmp_freg(mode, lhs, rhs);
                    self.masm.set(dest.reg(), cond_code);
                } else {
                    unimplemented!();
                }
            }

            Intrinsic::FloatAdd => self.masm.float_add(Float32, dest.freg(), lhs, rhs),
            Intrinsic::FloatSub => self.masm.float_sub(Float32, dest.freg(), lhs, rhs),
            Intrinsic::FloatMul => self.masm.float_mul(Float32, dest.freg(), lhs, rhs),
            Intrinsic::FloatDiv => self.masm.float_div(Float32, dest.freg(), lhs, rhs),

            Intrinsic::DoubleAdd => self.masm.float_add(Float64, dest.freg(), lhs, rhs),
            Intrinsic::DoubleSub => self.masm.float_sub(Float64, dest.freg(), lhs, rhs),
            Intrinsic::DoubleMul => self.masm.float_mul(Float64, dest.freg(), lhs, rhs),
            Intrinsic::DoubleDiv => self.masm.float_div(Float64, dest.freg(), lhs, rhs),

            _ => panic!("unexpected intrinsic {:?}", intr),
        }
    }

    fn emit_delegation(&mut self, e: &'ast ExprDelegationType, dest: ExprStore) {
        self.emit_universal_call(e.id, e.pos, dest);
    }

    fn has_call_site(&self, id: NodeId) -> bool {
        self.src.map_csites.get(id).is_some()
    }

    fn emit_universal_call(&mut self, id: NodeId, pos: Position, dest: ExprStore) {
        let csite = self.src.map_csites.get(id).unwrap().clone();
        let mut temps: Vec<(BuiltinType, i32)> = Vec::new();

        for arg in &csite.args {
            match *arg {
                Arg::Expr(ast, _, _) => {
                    self.emit_expr(ast, REG_RESULT.into());
                }

                Arg::Selfie(_, _) => {
                    self.emit_self(REG_RESULT);
                }

                Arg::SelfieNew(cls_id, _) => {
                    // allocate storage for object
                    self.masm.emit_comment(Comment::Alloc(cls_id));

                    let cls = self.ctxt.classes[cls_id].borrow();
                    self.masm.load_int_const(MachineMode::Int32, REG_PARAMS[0], cls.size as i64);

                    let mptr = stdlib::gc_alloc as *mut u8;
                    self.emit_native_call_insn(mptr, pos, BuiltinType::Ptr, 1, dest);

                    self.masm.test_if_nil_bailout(pos, dest.reg(), Trap::OOM);


                    // store classptr in object
                    let cptr = (&**cls.vtable.as_ref().unwrap()) as *const VTable as *const u8;
                    let disp = self.masm.add_addr(cptr);
                    let pos = self.masm.pos() as i32;

                    self.masm.emit_comment(Comment::StoreVTable(cls_id));
                    self.masm.load_constpool(REG_TMP1, disp + pos);
                    self.masm.store_mem(MachineMode::Ptr, Mem::Base(REG_RESULT, 0), REG_TMP1);
                }
            }

            let offset = self.reserve_temp_for_arg(arg);
            self.masm.store_mem(arg.ty().mode(), Mem::Local(offset), REG_RESULT);
            temps.push((arg.ty(), offset));
        }

        let mut arg_offset = -self.src.stacksize();

        for (ind, arg) in csite.args.iter().enumerate() {
            let ty = arg.ty();
            let offset = temps[ind].1;

            if ind < REG_PARAMS.len() {
                let reg = REG_PARAMS[ind];
                self.masm.load_mem(ty.mode(), reg, Mem::Local(offset));

                if ind == 0 {
                    let call_type = self.src.map_calls.get(id);

                    if call_type.is_some() && call_type.unwrap().is_method() && check_for_nil(ty) {
                        self.masm.test_if_nil_bailout(pos, reg, Trap::NIL);
                    }
                }

            } else {
                self.masm.load_mem(ty.mode(), REG_TMP1, Mem::Local(offset));
                self.masm.store_mem(ty.mode(), Mem::Local(arg_offset), REG_TMP1);

                arg_offset += 8;
            }
        }

        match csite.callee {
            Callee::Fct(fid) => {
                let fct = self.ctxt.fcts[fid].borrow();

                if csite.super_call {
                    let ptr = self.ptr_for_fct_id(fid);
                    self.masm.emit_comment(Comment::CallSuper(fid));
                    self.emit_direct_call_insn(fid, ptr, pos, csite.return_type, dest);

                } else if fct.is_virtual() {
                    let vtable_index = fct.vtable_index.unwrap();
                    self.masm.emit_comment(Comment::CallVirtual(fid));
                    self.emit_indirect_call_insn(vtable_index, pos, csite.return_type, dest);

                } else {
                    let ptr = self.ptr_for_fct_id(fid);
                    self.masm.emit_comment(Comment::CallDirect(fid));
                    self.emit_direct_call_insn(fid, ptr, pos, csite.return_type, dest);
                }
            }

            Callee::Ptr(ptr) => {
                self.emit_native_call_insn(ptr,
                                           pos,
                                           csite.return_type,
                                           csite.args.len() as i32,
                                           dest);
            }
        }

        if csite.args.len() > 0 {
            if let Arg::SelfieNew(_, _) = csite.args[0] {
                let (ty, offset) = temps[0];
                self.masm.load_mem(ty.mode(), dest.reg(), Mem::Local(offset));
            }
        }

        for temp in temps.into_iter() {
            self.free_temp_with_type(temp.0, temp.1);
        }
    }

    fn emit_native_call_insn(&mut self,
                             ptr: *const u8,
                             pos: Position,
                             ty: BuiltinType,
                             args: i32,
                             dest: ExprStore) {
        let ptr = ensure_native_stub(self.ctxt, FctId(0), ptr, ty, args);
        self.emit_direct_call_insn(FctId(0), ptr, pos, ty, dest);
    }

    fn emit_direct_call_insn(&mut self,
                             fid: FctId,
                             ptr: *const u8,
                             pos: Position,
                             ty: BuiltinType,
                             dest: ExprStore) {
        self.masm.direct_call(fid, ptr);
        self.emit_after_call_insns(pos, ty, dest);
    }

    fn emit_indirect_call_insn(&mut self,
                               index: u32,
                               pos: Position,
                               ty: BuiltinType,
                               dest: ExprStore) {
        self.masm.indirect_call(index);
        self.emit_after_call_insns(pos, ty, dest);
    }

    fn emit_after_call_insns(&mut self, pos: Position, ty: BuiltinType, dest: ExprStore) {
        self.masm.emit_lineno(pos.line as i32);

        let gcpoint = codegen::create_gcpoint(self.scopes, &self.temps);
        self.masm.emit_gcpoint(gcpoint);

        let dest = dest.reg();

        if REG_RESULT != dest {
            self.masm.copy_reg(ty.mode(), dest, REG_RESULT);
        }
    }
}

fn check_for_nil(ty: BuiltinType) -> bool {
    match ty {
        BuiltinType::Unit => false,
        BuiltinType::Str => true,
        BuiltinType::Byte | BuiltinType::Int | BuiltinType::Long | BuiltinType::Float |
        BuiltinType::Double | BuiltinType::Bool => false,
        BuiltinType::Nil | BuiltinType::Ptr | BuiltinType::ByteArray | BuiltinType::IntArray |
        BuiltinType::LongArray => true,
        BuiltinType::Class(_) => true,
        BuiltinType::Struct(_) => false,
    }
}

fn ensure_native_stub(ctxt: &Context,
                      fct_id: FctId,
                      ptr: *const u8,
                      ty: BuiltinType,
                      args: i32)
                      -> *const u8 {
    let mut native_fcts = ctxt.native_fcts.lock().unwrap();

    if let Some(ptr) = native_fcts.find_fct(ptr) {
        ptr

    } else {
        let jit_fct = native::generate(ctxt, fct_id, ptr, ty, args);
        let fct = ctxt.fcts[fct_id].borrow();

        if should_emit_asm(ctxt, &*fct) {
            dump_asm(ctxt,
                     &*fct,
                     &jit_fct,
                     None,
                     ctxt.args.flag_asm_syntax.unwrap_or(AsmSyntax::Att));
        }

        native_fcts.insert_fct(ptr, jit_fct)
    }
}

fn ensure_jit_or_stub_ptr<'ast>(src: &mut FctSrc<'ast>, ctxt: &Context) -> *const u8 {
    if let Some(ref jit) = src.jit_fct {
        return jit.fct_ptr();
    }

    ensure_stub(ctxt)
}

fn to_cond_code(cmp: CmpOp) -> CondCode {
    match cmp {
        CmpOp::Eq => CondCode::Equal,
        CmpOp::Ne => CondCode::NotEqual,
        CmpOp::Gt => CondCode::Greater,
        CmpOp::Ge => CondCode::GreaterEq,
        CmpOp::Lt => CondCode::Less,
        CmpOp::Le => CondCode::LessEq,
        CmpOp::Is => CondCode::Equal,
        CmpOp::IsNot => CondCode::NotEqual,
    }
}
