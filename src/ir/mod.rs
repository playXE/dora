pub mod builder;

use ast::{BinOp, UnOp};

pub struct Fct {
    blocks: Vec<Block>,
    start_id: BlockId,
    end_ids: Vec<BlockId>,
    vars: Vec<Var>,
    params: Vec<Var>,
}

impl Fct {
    pub fn new() -> Fct {
        Fct {
            blocks: Vec::new(),
            start_id: BlockId(0),
            end_ids: Vec::new(),
            vars: Vec::new(),
            params: Vec::new(),
        }
    }

    pub fn block_mut(&mut self, id: BlockId) -> &mut Block {
        &mut self.blocks[id.0]
    }

    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0]
    }

    pub fn add_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len());
        self.blocks.push(Block::new(id));

        id
    }
}

#[derive(Copy, Clone)]
pub struct VarId(usize);

pub struct Var {
    id: VarId,
    name: String,
}

#[derive(Copy, Clone)]
pub struct BlockId(usize);

pub struct Block {
    id: BlockId,
    instructions: Vec<Instr>,
    successors: Vec<BlockId>,
    predecessors: Vec<BlockId>,
}

impl Block {
    fn new(id: BlockId) -> Block {
        Block {
            id: id,
            instructions: Vec::new(),
            successors: Vec::new(),
            predecessors: Vec::new()
        }
    }

    fn add_instr(&mut self, instr: Instr) {
        self.instructions.push(instr);
    }

    fn add_predecessor(&mut self, id: BlockId) {
        self.predecessors.push(id);
    }

    fn add_successor(&mut self, id: BlockId) {
        self.successors.push(id);
    }
}

pub enum Instr {
    InstrRet(Option<Opnd>),
    InstrTest(Opnd),
    InstrBin(Opnd, Opnd, BinOp, Opnd),
    InstrUn(Opnd, UnOp, Opnd),
    InstrAssign(Opnd, Opnd),
    InstrPhi(VarId, u32, Vec<u32>),
    InstrCall(String, Opnd, Vec<Opnd>),
    InstrStr(Opnd, String),
    InstrGoto(BlockId),
}

#[derive(Copy, Clone)]
pub enum Opnd {
    OpndReg(u32),
    OpndVar(VarId, u32),
    OpndInt(i32),
    OpndBool(bool),
}