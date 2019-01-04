use std::collections::HashMap;
use std::{iter, mem};

use failure::Fail;
use num_traits::cast;

use gc_arena::{Gc, MutationContext};

use crate::function::{FunctionProto, UpValueDescriptor};
use crate::opcode::{
    ConstantIndex16, ConstantIndex8, OpCode, PrototypeIndex, RegisterIndex, UpValueIndex, VarCount,
};
use crate::parser::{
    AssignmentStatement, AssignmentTarget, BinaryOperator, Block, CallSuffix, Chunk, Expression,
    FieldSuffix, FunctionCallStatement, FunctionDefinition, FunctionStatement, HeadExpression,
    IfStatement, LocalStatement, PrimaryExpression, ReturnStatement, SimpleExpression, Statement,
    SuffixPart, SuffixedExpression, TableConstructor, UnaryOperator,
};
use crate::string::String;
use crate::value::Value;

mod constant;
mod operators;
mod register_allocator;

use self::constant::ConstantValue;
use self::operators::{
    categorize_binop, comparison_binop_const_fold, comparison_binop_opcode,
    simple_binop_const_fold, simple_binop_opcode, unop_const_fold, unop_opcode, BinOpCategory,
    ComparisonBinOp, RegisterOrConstant, ShortCircuitBinOp,
};
use self::register_allocator::RegisterAllocator;

pub fn compile_chunk<'gc>(
    mc: MutationContext<'gc, '_>,
    chunk: &Chunk,
) -> Result<FunctionProto<'gc>, CompilerError> {
    let mut compiler = Compiler {
        mutation_context: mc,
        functions: Vec::new(),
    };
    compiler.compile_function(&[], true, &chunk.block)
}

#[derive(Fail, Debug)]
pub enum CompilerError {
    #[fail(display = "insufficient available registers")]
    Registers,
    #[fail(display = "too many upvalues")]
    UpValues,
    #[fail(display = "too many returns")]
    Returns,
    #[fail(display = "too many fixed parameters")]
    FixedParameters,
    #[fail(display = "too many inner functions")]
    Functions,
    #[fail(display = "too many constants")]
    Constants,
    #[fail(display = "too many opcodes")]
    OpCodes,
}

struct Compiler<'gc, 'a> {
    mutation_context: MutationContext<'gc, 'a>,
    functions: Vec<CompilerFunction<'gc, 'a>>,
}

impl<'gc, 'a> Compiler<'gc, 'a> {
    fn compile_function(
        &mut self,
        parameters: &'a [Box<[u8]>],
        has_varargs: bool,
        body: &'a Block,
    ) -> Result<FunctionProto<'gc>, CompilerError> {
        let mut new_function = CompilerFunction::default();
        let fixed_params: u8 = cast(parameters.len()).ok_or(CompilerError::FixedParameters)?;
        new_function.register_allocator.push(fixed_params);
        new_function.has_varargs = has_varargs;
        new_function.fixed_params = fixed_params;
        for (i, name) in parameters.iter().enumerate() {
            new_function
                .locals
                .push((name, RegisterIndex(cast(i).unwrap())));
        }

        self.functions.push(new_function);
        self.block(&body)?;
        let mut new_function = self.functions.pop().unwrap();

        new_function.opcodes.push(OpCode::Return {
            start: RegisterIndex(0),
            count: VarCount::make_zero(),
        });
        assert!(new_function.locals.len() == fixed_params as usize);
        for (_, r) in new_function.locals.drain(..) {
            new_function.register_allocator.free(r);
        }
        assert_eq!(
            new_function.register_allocator.stack_top(),
            0,
            "register leak detected"
        );

        Ok(FunctionProto {
            fixed_params: new_function.fixed_params,
            has_varargs: false,
            stack_size: new_function.register_allocator.stack_size(),
            constants: new_function.constants,
            opcodes: new_function.opcodes,
            upvalues: new_function.upvalues.iter().map(|(_, d)| *d).collect(),
            prototypes: new_function
                .prototypes
                .into_iter()
                .map(|f| Gc::allocate(self.mutation_context, f))
                .collect(),
        })
    }

    fn block(&mut self, block: &'a Block) -> Result<(), CompilerError> {
        let first_local = self.current_function().locals.len();

        for statement in &block.statements {
            self.statement(statement)?;
        }

        if let Some(return_statement) = &block.return_statement {
            self.return_statement(return_statement)?;
        }

        let current_function = self.current_function();
        for (_, r) in current_function.locals.drain(first_local..) {
            current_function.register_allocator.free(r);
        }

        Ok(())
    }

    fn statement(&mut self, statement: &'a Statement) -> Result<(), CompilerError> {
        match statement {
            Statement::If(if_statement) => self.if_statement(if_statement),
            Statement::While(_) => panic!("while statement unsupported"),
            Statement::Do(block) => self.block(block),
            Statement::For(_) => panic!("for statement unsupported"),
            Statement::Repeat(_) => panic!("repeat statement unsupported"),
            Statement::Function(function_statement) => self.function_statement(function_statement),
            Statement::LocalFunction(local_function) => self.local_function(local_function),
            Statement::LocalStatement(local_statement) => self.local_statement(local_statement),
            Statement::Label(_) => panic!("label statement unsupported"),
            Statement::Break => panic!("break statement unsupported"),
            Statement::Goto(_) => panic!("goto statement unsupported"),
            Statement::FunctionCall(function_call) => self.function_call(function_call),
            Statement::Assignment(assignment) => self.assignment(assignment),
        }
    }

    fn if_statement(&mut self, if_statement: &'a IfStatement) -> Result<(), CompilerError> {
        let mut end_jumps = Vec::new();
        let mut next_jump = None;

        for (if_expr, block) in iter::once(&if_statement.if_part).chain(&if_statement.else_if_parts)
        {
            if let Some(next_jump) = next_jump.take() {
                self.jump_here(next_jump)?;
            }

            match self.expression(if_expr)? {
                ExprDescriptor::Comparison {
                    mut left,
                    op,
                    mut right,
                } => {
                    let left_reg_cons = self.expr_any_register_or_constant(&mut left)?;
                    let right_reg_cons = self.expr_any_register_or_constant(&mut right)?;
                    self.expr_discharge(*left, ExprDestination::None)?;
                    self.expr_discharge(*right, ExprDestination::None)?;

                    let comparison_opcode =
                        comparison_binop_opcode(op, left_reg_cons, right_reg_cons);
                    self.current_function().opcodes.push(comparison_opcode);
                }

                ExprDescriptor::Not(mut test_expr) => {
                    let test_reg = self.expr_any_register(&mut test_expr)?;
                    self.expr_discharge(*test_expr, ExprDestination::None)?;
                    self.current_function().opcodes.push(OpCode::Test {
                        value: test_reg,
                        is_true: false,
                    });
                }

                mut test_expr => {
                    let test_reg = self.expr_any_register(&mut test_expr)?;
                    self.expr_discharge(test_expr, ExprDestination::None)?;
                    self.current_function().opcodes.push(OpCode::Test {
                        value: test_reg,
                        is_true: true,
                    });
                }
            }

            next_jump = Some(self.jump_placeholder());
            self.block(block)?;
            end_jumps.push(self.jump_placeholder());
        }

        if let Some(else_block) = &if_statement.else_part {
            if let Some(next_jump) = next_jump.take() {
                self.jump_here(next_jump)?;
            }
            self.block(else_block)?;
        }

        for end_jump in end_jumps.into_iter().chain(next_jump) {
            self.jump_here(end_jump)?;
        }

        Ok(())
    }

    fn function_statement(
        &mut self,
        function_statement: &'a FunctionStatement,
    ) -> Result<(), CompilerError> {
        if !function_statement.name.fields.is_empty() {
            panic!("no function name fields support");
        }
        if function_statement.name.method.is_some() {
            panic!("no method support");
        }

        let proto = self.new_prototype(&function_statement.definition)?;

        let mut env = self.get_environment()?;
        let mut name = ExprDescriptor::Value(Value::String(String::new(
            self.mutation_context,
            &*function_statement.name.name,
        )));

        let dest = self
            .current_function()
            .register_allocator
            .allocate()
            .ok_or(CompilerError::Registers)?;
        self.current_function()
            .opcodes
            .push(OpCode::Closure { proto, dest });
        let mut closure = ExprDescriptor::Register {
            register: dest,
            is_temporary: true,
        };

        self.set_table(&mut env, &mut name, &mut closure)?;

        self.expr_discharge(env, ExprDestination::None)?;
        self.expr_discharge(name, ExprDestination::None)?;
        self.expr_discharge(closure, ExprDestination::None)?;

        Ok(())
    }

    fn return_statement(
        &mut self,
        return_statement: &'a ReturnStatement,
    ) -> Result<(), CompilerError> {
        let ret_len = return_statement.returns.len();

        if ret_len == 0 {
            self.current_function().opcodes.push(OpCode::Return {
                start: RegisterIndex(0),
                count: VarCount::make_zero(),
            });
        } else {
            let ret_start = cast(self.current_function().register_allocator.stack_top())
                .ok_or(CompilerError::Registers)?;

            for i in 0..ret_len - 1 {
                let expr = self.expression(&return_statement.returns[i])?;
                self.expr_discharge(expr, ExprDestination::PushNew)?;
            }

            let ret_count = match self.expression(&return_statement.returns[ret_len - 1])? {
                ExprDescriptor::FunctionCall { func, args } => {
                    self.expr_function_call(*func, args, VarCount::make_variable())?;
                    VarCount::make_variable()
                }
                expr => {
                    self.expr_discharge(expr, ExprDestination::PushNew)?;
                    cast(ret_len)
                        .and_then(VarCount::make_constant)
                        .ok_or(CompilerError::Returns)?
                }
            };

            self.current_function().opcodes.push(OpCode::Return {
                start: RegisterIndex(ret_start),
                count: ret_count,
            });

            // Free all allocated return registers so that we do not fail the register leak check
            self.current_function()
                .register_allocator
                .pop_to(ret_start as u16);
        }

        Ok(())
    }

    fn local_statement(
        &mut self,
        local_statement: &'a LocalStatement,
    ) -> Result<(), CompilerError> {
        let name_len = local_statement.names.len();
        let val_len = local_statement.values.len();

        for i in 0..val_len {
            let expr = self.expression(&local_statement.values[i])?;

            if i >= name_len {
                self.expr_discharge(expr, ExprDestination::None)?;
            } else if i == val_len - 1 {
                match expr {
                    ExprDescriptor::FunctionCall { func, args } => {
                        let num_returns =
                            cast(1 + name_len - val_len).ok_or(CompilerError::Registers)?;
                        self.expr_function_call(
                            *func,
                            args,
                            VarCount::make_constant(num_returns).ok_or(CompilerError::Returns)?,
                        )?;

                        let reg = self
                            .current_function()
                            .register_allocator
                            .push(num_returns)
                            .ok_or(CompilerError::Registers)?;
                        for j in 0..num_returns {
                            self.current_function().locals.push((
                                &local_statement.names[i + j as usize],
                                RegisterIndex(reg.0 + j),
                            ));
                        }

                        return Ok(());
                    }
                    expr => {
                        let reg = self
                            .expr_discharge(expr, ExprDestination::AllocateNew)?
                            .unwrap();
                        self.current_function()
                            .locals
                            .push((&local_statement.names[i], reg));
                    }
                }
            } else {
                let reg = self
                    .expr_discharge(expr, ExprDestination::AllocateNew)?
                    .unwrap();
                self.current_function()
                    .locals
                    .push((&local_statement.names[i], reg));
            }
        }

        for i in local_statement.values.len()..local_statement.names.len() {
            let reg = self
                .current_function()
                .register_allocator
                .allocate()
                .ok_or(CompilerError::Registers)?;
            self.load_nil(reg)?;
            self.current_function()
                .locals
                .push((&local_statement.names[i], reg));
        }

        Ok(())
    }

    fn function_call(
        &mut self,
        function_call: &'a FunctionCallStatement,
    ) -> Result<(), CompilerError> {
        let func_expr = self.suffixed_expression(&function_call.head)?;
        match &function_call.call {
            CallSuffix::Function(args) => {
                let arg_exprs = args
                    .iter()
                    .map(|arg| self.expression(arg))
                    .collect::<Result<_, CompilerError>>()?;
                self.expr_function_call(func_expr, arg_exprs, VarCount::make_zero())?;
            }
            CallSuffix::Method(_, _) => panic!("method call unsupported"),
        }
        Ok(())
    }

    fn assignment(&mut self, assignment: &'a AssignmentStatement) -> Result<(), CompilerError> {
        for (i, target) in assignment.targets.iter().enumerate() {
            let mut expr = if i < assignment.values.len() {
                self.expression(&assignment.values[i])?
            } else {
                ExprDescriptor::Value(Value::Nil)
            };

            match target {
                AssignmentTarget::Name(name) => match self.find_variable(name)? {
                    VariableDescriptor::Local(dest) => {
                        self.expr_discharge(expr, ExprDestination::Register(dest))?;
                    }
                    VariableDescriptor::UpValue(dest) => {
                        let source = self.expr_any_register(&mut expr)?;
                        self.current_function()
                            .opcodes
                            .push(OpCode::SetUpValue { source, dest });
                        self.expr_discharge(expr, ExprDestination::None)?;
                    }
                    VariableDescriptor::Global(name) => {
                        let mut env = self.get_environment()?;
                        let mut key = ExprDescriptor::Value(Value::String(String::new(
                            self.mutation_context,
                            name,
                        )));
                        self.set_table(&mut env, &mut key, &mut expr)?;
                        self.expr_discharge(env, ExprDestination::None)?;
                        self.expr_discharge(key, ExprDestination::None)?;
                        self.expr_discharge(expr, ExprDestination::None)?;
                    }
                },

                AssignmentTarget::Field(table, field) => {
                    let mut table = self.suffixed_expression(table)?;
                    let mut key = match field {
                        FieldSuffix::Named(name) => ExprDescriptor::Value(Value::String(
                            String::new(self.mutation_context, name),
                        )),
                        FieldSuffix::Indexed(idx) => self.expression(idx)?,
                    };
                    self.set_table(&mut table, &mut key, &mut expr)?;
                    self.expr_discharge(table, ExprDestination::None)?;
                    self.expr_discharge(key, ExprDestination::None)?;
                    self.expr_discharge(expr, ExprDestination::None)?;
                }
            }
        }

        Ok(())
    }

    fn local_function(
        &mut self,
        local_function: &'a FunctionStatement,
    ) -> Result<(), CompilerError> {
        if !local_function.name.fields.is_empty() {
            panic!("no function name fields support");
        }
        if local_function.name.method.is_some() {
            panic!("no method support");
        }

        let proto = self.new_prototype(&local_function.definition)?;

        let dest = self
            .current_function()
            .register_allocator
            .allocate()
            .ok_or(CompilerError::Registers)?;
        self.current_function()
            .opcodes
            .push(OpCode::Closure { proto, dest });
        self.current_function()
            .locals
            .push((&local_function.name.name, dest));

        Ok(())
    }

    fn expression(
        &mut self,
        expression: &'a Expression,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        let mut expr = self.head_expression(&expression.head)?;
        for (binop, right) in &expression.tail {
            expr = self.binary_operator(expr, *binop, right)?;
        }
        Ok(expr)
    }

    fn head_expression(
        &mut self,
        head_expression: &'a HeadExpression,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        match head_expression {
            HeadExpression::Simple(simple_expression) => self.simple_expression(simple_expression),
            HeadExpression::UnaryOperator(unop, expr) => {
                let expr = self.expression(expr)?;
                self.unary_operator(*unop, expr)
            }
        }
    }

    fn simple_expression(
        &mut self,
        simple_expression: &'a SimpleExpression,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        Ok(match simple_expression {
            SimpleExpression::Float(f) => ExprDescriptor::Value(Value::Number(*f)),
            SimpleExpression::Integer(i) => ExprDescriptor::Value(Value::Integer(*i)),
            SimpleExpression::String(s) => {
                let string = String::new(self.mutation_context, &*s);
                ExprDescriptor::Value(Value::String(string))
            }
            SimpleExpression::Nil => ExprDescriptor::Value(Value::Nil),
            SimpleExpression::True => ExprDescriptor::Value(Value::Boolean(true)),
            SimpleExpression::False => ExprDescriptor::Value(Value::Boolean(false)),
            SimpleExpression::VarArgs => panic!("varargs expression unsupported"),
            SimpleExpression::TableConstructor(table_constructor) => {
                self.table_constructor(table_constructor)?
            }
            SimpleExpression::Function(function) => self.function_expression(function)?,
            SimpleExpression::Suffixed(suffixed) => self.suffixed_expression(suffixed)?,
        })
    }

    fn table_constructor(
        &mut self,
        table_constructor: &'a TableConstructor,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        if !table_constructor.fields.is_empty() {
            panic!("only empty table constructors supported");
        }

        let dest = self
            .current_function()
            .register_allocator
            .allocate()
            .ok_or(CompilerError::Registers)?;
        self.current_function()
            .opcodes
            .push(OpCode::NewTable { dest });

        Ok(ExprDescriptor::Register {
            register: dest,
            is_temporary: true,
        })
    }

    fn function_expression(
        &mut self,
        function: &'a FunctionDefinition,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        let proto = self.new_prototype(function)?;
        let dest = self
            .current_function()
            .register_allocator
            .allocate()
            .ok_or(CompilerError::Registers)?;
        self.current_function()
            .opcodes
            .push(OpCode::Closure { proto, dest });

        Ok(ExprDescriptor::Register {
            register: dest,
            is_temporary: true,
        })
    }

    fn suffixed_expression(
        &mut self,
        suffixed_expression: &'a SuffixedExpression,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        let mut expr = self.primary_expression(&suffixed_expression.primary)?;
        for suffix in &suffixed_expression.suffixes {
            match suffix {
                SuffixPart::Field(field) => {
                    let mut key = match field {
                        FieldSuffix::Named(name) => ExprDescriptor::Value(Value::String(
                            String::new(self.mutation_context, name),
                        )),
                        FieldSuffix::Indexed(idx) => self.expression(idx)?,
                    };
                    let res = self.get_table(&mut expr, &mut key)?;
                    self.expr_discharge(expr, ExprDestination::None)?;
                    self.expr_discharge(key, ExprDestination::None)?;
                    expr = res;
                }
                SuffixPart::Call(call_suffix) => match call_suffix {
                    CallSuffix::Function(args) => {
                        let args = args
                            .iter()
                            .map(|arg| self.expression(arg))
                            .collect::<Result<_, CompilerError>>()?;
                        expr = ExprDescriptor::FunctionCall {
                            func: Box::new(expr),
                            args,
                        };
                    }
                    CallSuffix::Method(_, _) => panic!("methods not supported yet"),
                },
            }
        }
        Ok(expr)
    }

    fn primary_expression(
        &mut self,
        primary_expression: &'a PrimaryExpression,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        match primary_expression {
            PrimaryExpression::Name(name) => Ok(match self.find_variable(name)? {
                VariableDescriptor::Local(register) => ExprDescriptor::Register {
                    register,
                    is_temporary: false,
                },
                VariableDescriptor::UpValue(upvalue) => ExprDescriptor::UpValue(upvalue),
                VariableDescriptor::Global(name) => {
                    let mut env = self.get_environment()?;
                    let mut key = ExprDescriptor::Value(Value::String(String::new(
                        self.mutation_context,
                        name,
                    )));
                    let res = self.get_table(&mut env, &mut key)?;
                    self.expr_discharge(env, ExprDestination::None)?;
                    self.expr_discharge(key, ExprDestination::None)?;
                    res
                }
            }),
            PrimaryExpression::GroupedExpression(expr) => self.expression(expr),
        }
    }

    // Returns the function currently being compiled
    fn current_function(&mut self) -> &mut CompilerFunction<'gc, 'a> {
        self.functions.last_mut().expect("no current function")
    }

    fn new_prototype(
        &mut self,
        function: &'a FunctionDefinition,
    ) -> Result<PrototypeIndex, CompilerError> {
        let proto =
            self.compile_function(&function.parameters, function.has_varargs, &function.body)?;
        self.current_function().prototypes.push(proto);
        Ok(PrototypeIndex(
            cast(self.current_function().prototypes.len() - 1).ok_or(CompilerError::Functions)?,
        ))
    }

    fn unary_operator(
        &mut self,
        unop: UnaryOperator,
        mut expr: ExprDescriptor<'gc, 'a>,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        if let &ExprDescriptor::Value(v) = &expr {
            if let Some(v) = unop_const_fold(unop, v) {
                return Ok(ExprDescriptor::Value(v));
            }
        }

        if unop == UnaryOperator::Not {
            return Ok(ExprDescriptor::Not(Box::new(expr)));
        }

        let source = self.expr_any_register(&mut expr)?;
        self.expr_discharge(expr, ExprDestination::None)?;

        let dest = self
            .current_function()
            .register_allocator
            .allocate()
            .ok_or(CompilerError::Registers)?;
        let unop_opcode = unop_opcode(unop, dest, source);
        self.current_function().opcodes.push(unop_opcode);
        Ok(ExprDescriptor::Register {
            register: dest,
            is_temporary: true,
        })
    }

    fn binary_operator(
        &mut self,
        mut left: ExprDescriptor<'gc, 'a>,
        binop: BinaryOperator,
        right: &'a Expression,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        match categorize_binop(binop) {
            BinOpCategory::Simple(op) => {
                let mut right = self.expression(right)?;

                if let (&ExprDescriptor::Value(a), &ExprDescriptor::Value(b)) = (&left, &right) {
                    if let Some(v) = simple_binop_const_fold(op, a, b) {
                        return Ok(ExprDescriptor::Value(v));
                    }
                }

                let left_reg_cons = self.expr_any_register_or_constant(&mut left)?;
                let right_reg_cons = self.expr_any_register_or_constant(&mut right)?;
                self.expr_discharge(left, ExprDestination::None)?;
                self.expr_discharge(right, ExprDestination::None)?;

                let dest = self
                    .current_function()
                    .register_allocator
                    .allocate()
                    .ok_or(CompilerError::Registers)?;
                let simple_binop_opcode =
                    simple_binop_opcode(op, dest, left_reg_cons, right_reg_cons);
                self.current_function().opcodes.push(simple_binop_opcode);

                Ok(ExprDescriptor::Register {
                    register: dest,
                    is_temporary: true,
                })
            }

            BinOpCategory::Comparison(op) => {
                let right = self.expression(right)?;

                if let (&ExprDescriptor::Value(a), &ExprDescriptor::Value(b)) = (&left, &right) {
                    if let Some(v) = comparison_binop_const_fold(op, a, b) {
                        return Ok(ExprDescriptor::Value(v));
                    }
                }

                Ok(ExprDescriptor::Comparison {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                })
            }

            BinOpCategory::ShortCircuit(op) => Ok(ExprDescriptor::ShortCircuitBinOp {
                left: Box::new(left),
                op,
                right,
            }),

            BinOpCategory::Concat => panic!("no support for concat operator"),
        }
    }

    fn find_variable(&mut self, name: &'a [u8]) -> Result<VariableDescriptor<'a>, CompilerError> {
        let function_len = self.functions.len();

        for i in (0..function_len).rev() {
            for j in (0..self.functions[i].locals.len()).rev() {
                let (local_name, register) = self.functions[i].locals[j];
                if name == local_name {
                    if i == function_len - 1 {
                        return Ok(VariableDescriptor::Local(register));
                    } else {
                        self.functions[i + 1]
                            .upvalues
                            .push((name, UpValueDescriptor::ParentLocal(register)));
                        let mut upvalue_index = UpValueIndex(
                            cast(self.functions[i + 1].upvalues.len() - 1)
                                .ok_or(CompilerError::UpValues)?,
                        );
                        for k in i + 2..function_len {
                            self.functions[k]
                                .upvalues
                                .push((name, UpValueDescriptor::Outer(upvalue_index)));
                            upvalue_index = UpValueIndex(
                                cast(self.functions[k].upvalues.len() - 1)
                                    .ok_or(CompilerError::UpValues)?,
                            );
                        }
                        return Ok(VariableDescriptor::UpValue(upvalue_index));
                    }
                }
            }

            // The top-level function has an implicit _ENV upvalue (and this is the only upvalue it
            // can have), we add it if it is ever referenced.
            if i == 0 && name == b"_ENV" && self.functions[i].upvalues.is_empty() {
                self.functions[0]
                    .upvalues
                    .push((b"_ENV", UpValueDescriptor::Environment));
            }

            for j in 0..self.functions[i].upvalues.len() {
                if name == self.functions[i].upvalues[j].0 {
                    let mut upvalue_index = UpValueIndex(cast(j).unwrap());
                    if i == function_len - 1 {
                        return Ok(VariableDescriptor::UpValue(upvalue_index));
                    } else {
                        for k in i + 1..function_len {
                            self.functions[k]
                                .upvalues
                                .push((name, UpValueDescriptor::Outer(upvalue_index)));
                            upvalue_index = UpValueIndex(
                                cast(self.functions[k].upvalues.len() - 1)
                                    .ok_or(CompilerError::UpValues)?,
                            );
                        }
                        return Ok(VariableDescriptor::UpValue(upvalue_index));
                    }
                }
            }
        }

        Ok(VariableDescriptor::Global(name))
    }

    // Get a reference to the variable _ENV in scope, or if that is not in scope, the implicit chunk
    // _ENV.
    fn get_environment(&mut self) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        Ok(match self.find_variable(b"_ENV")? {
            VariableDescriptor::Local(register) => ExprDescriptor::Register {
                register,
                is_temporary: false,
            },
            VariableDescriptor::UpValue(upvalue) => ExprDescriptor::UpValue(upvalue),
            VariableDescriptor::Global(_) => unreachable!("there should always be an _ENV upvalue"),
        })
    }

    // Emit a LoadNil opcode, possibly combining several sequential LoadNil opcodes into one.
    fn load_nil(&mut self, dest: RegisterIndex) -> Result<(), CompilerError> {
        match self.current_function().opcodes.last().cloned() {
            Some(OpCode::LoadNil {
                dest: prev_dest,
                count: prev_count,
            }) if prev_dest.0 + prev_count == dest.0 => {
                self.current_function().opcodes.push(OpCode::LoadNil {
                    dest: prev_dest,
                    count: prev_count + 1,
                });
            }
            _ => {
                self.current_function()
                    .opcodes
                    .push(OpCode::LoadNil { dest, count: 1 });
            }
        }
        Ok(())
    }

    // Places a placeholdeer jump instruction that must be updated later with jump_finish
    fn jump_placeholder(&mut self) -> usize {
        let jmp_inst = self.current_function().opcodes.len();
        self.current_function()
            .opcodes
            .push(OpCode::Jump { offset: 0 });
        jmp_inst
    }

    // Commit a placeholder jump to jump to the next instruction
    fn jump_here(&mut self, jmp_instruction: usize) -> Result<(), CompilerError> {
        let jmp_offset = cast(self.current_function().opcodes.len() - jmp_instruction - 1)
            .ok_or(CompilerError::OpCodes)?;
        if let OpCode::Jump { offset } = &mut self.current_function().opcodes[jmp_instruction] {
            if *offset == 0 {
                *offset = jmp_offset;
                return Ok(());
            }
        }
        panic!("jump instruction is not a placeholder jump instruction");
    }

    fn get_constant(&mut self, constant: Value<'gc>) -> Result<ConstantIndex16, CompilerError> {
        if let Some(constant) = self
            .current_function()
            .constant_table
            .get(&ConstantValue(constant))
            .cloned()
        {
            Ok(constant)
        } else {
            let c = ConstantIndex16(
                cast(self.current_function().constants.len()).ok_or(CompilerError::Constants)?,
            );
            self.current_function().constants.push(constant);
            self.current_function()
                .constant_table
                .insert(ConstantValue(constant), c);
            Ok(c)
        }
    }

    fn get_table(
        &mut self,
        table: &mut ExprDescriptor<'gc, 'a>,
        key: &mut ExprDescriptor<'gc, 'a>,
    ) -> Result<ExprDescriptor<'gc, 'a>, CompilerError> {
        let dest = self
            .current_function()
            .register_allocator
            .allocate()
            .ok_or(CompilerError::Registers)?;
        let op = match table {
            &mut ExprDescriptor::UpValue(table) => match self.expr_any_register_or_constant(key)? {
                RegisterOrConstant::Constant(key) => OpCode::GetUpTableC { dest, table, key },
                RegisterOrConstant::Register(key) => OpCode::GetUpTableR { dest, table, key },
            },
            table => {
                let table = self.expr_any_register(table)?;
                match self.expr_any_register_or_constant(key)? {
                    RegisterOrConstant::Constant(key) => OpCode::GetTableC { dest, table, key },
                    RegisterOrConstant::Register(key) => OpCode::GetTableR { dest, table, key },
                }
            }
        };

        self.current_function().opcodes.push(op);
        Ok(ExprDescriptor::Register {
            register: dest,
            is_temporary: true,
        })
    }

    fn set_table(
        &mut self,
        table: &mut ExprDescriptor<'gc, 'a>,
        key: &mut ExprDescriptor<'gc, 'a>,
        value: &mut ExprDescriptor<'gc, 'a>,
    ) -> Result<(), CompilerError> {
        let op = match table {
            &mut ExprDescriptor::UpValue(table) => {
                match (
                    self.expr_any_register_or_constant(key)?,
                    self.expr_any_register_or_constant(value)?,
                ) {
                    (RegisterOrConstant::Register(key), RegisterOrConstant::Register(value)) => {
                        OpCode::SetUpTableRR { table, key, value }
                    }
                    (RegisterOrConstant::Register(key), RegisterOrConstant::Constant(value)) => {
                        OpCode::SetUpTableRC { table, key, value }
                    }
                    (RegisterOrConstant::Constant(key), RegisterOrConstant::Register(value)) => {
                        OpCode::SetUpTableCR { table, key, value }
                    }
                    (RegisterOrConstant::Constant(key), RegisterOrConstant::Constant(value)) => {
                        OpCode::SetUpTableCC { table, key, value }
                    }
                }
            }
            table => {
                let table = self.expr_any_register(table)?;
                match (
                    self.expr_any_register_or_constant(key)?,
                    self.expr_any_register_or_constant(value)?,
                ) {
                    (RegisterOrConstant::Register(key), RegisterOrConstant::Register(value)) => {
                        OpCode::SetTableRR { table, key, value }
                    }
                    (RegisterOrConstant::Register(key), RegisterOrConstant::Constant(value)) => {
                        OpCode::SetTableRC { table, key, value }
                    }
                    (RegisterOrConstant::Constant(key), RegisterOrConstant::Register(value)) => {
                        OpCode::SetTableCR { table, key, value }
                    }
                    (RegisterOrConstant::Constant(key), RegisterOrConstant::Constant(value)) => {
                        OpCode::SetTableCC { table, key, value }
                    }
                }
            }
        };

        self.current_function().opcodes.push(op);
        Ok(())
    }

    // Modify an expression so that it contains its result in any register, and return that
    // register.
    fn expr_any_register(
        &mut self,
        expr: &mut ExprDescriptor<'gc, 'a>,
    ) -> Result<RegisterIndex, CompilerError> {
        if let ExprDescriptor::Register { register, .. } = *expr {
            Ok(register)
        } else {
            // The given expresison will be invalid if `expr_discharge` errors, but this is fine,
            // compiler errors always halt compilation.
            let register = self
                .expr_discharge(
                    mem::replace(expr, ExprDescriptor::Value(Value::Nil)),
                    ExprDestination::AllocateNew,
                )?
                .unwrap();
            *expr = ExprDescriptor::Register {
                register,
                is_temporary: true,
            };
            Ok(register)
        }
    }

    // If the expression is a constant value *and* fits into an 8-bit constant index, return that
    // constant index, otherwise modify the expression so that it is contained in a register and
    // return that register.
    fn expr_any_register_or_constant(
        &mut self,
        expr: &mut ExprDescriptor<'gc, 'a>,
    ) -> Result<RegisterOrConstant, CompilerError> {
        if let &mut ExprDescriptor::Value(cons) = expr {
            if let Some(c8) = cast(self.get_constant(cons)?.0) {
                return Ok(RegisterOrConstant::Constant(ConstantIndex8(c8)));
            }
        }
        Ok(RegisterOrConstant::Register(self.expr_any_register(expr)?))
    }

    // Consume an expression, placing it in the given destination.  If the desitnation is not None,
    // then the resulting register is returned.  The returned register (if any) will always be
    // marked as allocated, so it must be placed into another expression or freed.
    fn expr_discharge(
        &mut self,
        expr: ExprDescriptor<'gc, 'a>,
        dest: ExprDestination,
    ) -> Result<Option<RegisterIndex>, CompilerError> {
        fn new_destination<'gc, 'a>(
            comp: &mut Compiler<'gc, 'a>,
            dest: ExprDestination,
        ) -> Result<Option<RegisterIndex>, CompilerError> {
            Ok(match dest {
                ExprDestination::Register(dest) => Some(dest),
                ExprDestination::AllocateNew => Some(
                    comp.current_function()
                        .register_allocator
                        .allocate()
                        .ok_or(CompilerError::Registers)?,
                ),
                ExprDestination::PushNew => Some(
                    comp.current_function()
                        .register_allocator
                        .push(1)
                        .ok_or(CompilerError::Registers)?,
                ),
                ExprDestination::None => None,
            })
        }

        let result = match expr {
            ExprDescriptor::Register {
                register: source,
                is_temporary,
            } => {
                if dest == ExprDestination::AllocateNew && is_temporary {
                    Some(source)
                } else {
                    if is_temporary {
                        self.current_function().register_allocator.free(source);
                    }
                    if let Some(dest) = new_destination(self, dest)? {
                        if dest != source {
                            self.current_function()
                                .opcodes
                                .push(OpCode::Move { dest, source });
                        }
                        Some(dest)
                    } else {
                        None
                    }
                }
            }

            ExprDescriptor::UpValue(source) => {
                if let Some(dest) = new_destination(self, dest)? {
                    self.current_function()
                        .opcodes
                        .push(OpCode::GetUpValue { source, dest });
                    Some(dest)
                } else {
                    None
                }
            }

            ExprDescriptor::Value(value) => {
                if let Some(dest) = new_destination(self, dest)? {
                    match value {
                        Value::Nil => {
                            self.load_nil(dest)?;
                        }
                        Value::Boolean(value) => {
                            self.current_function().opcodes.push(OpCode::LoadBool {
                                dest,
                                value,
                                skip_next: false,
                            });
                        }
                        val => {
                            let constant = self.get_constant(val)?;
                            self.current_function()
                                .opcodes
                                .push(OpCode::LoadConstant { dest, constant });
                        }
                    }
                    Some(dest)
                } else {
                    None
                }
            }

            ExprDescriptor::Not(mut expr) => {
                if let Some(dest) = new_destination(self, dest)? {
                    let source = self.expr_any_register(&mut expr)?;
                    self.expr_discharge(*expr, ExprDestination::None)?;
                    self.current_function()
                        .opcodes
                        .push(OpCode::Not { dest, source });
                    Some(dest)
                } else {
                    self.expr_discharge(*expr, ExprDestination::None)?;
                    None
                }
            }

            ExprDescriptor::FunctionCall { func, args } => {
                let source = self.expr_function_call(*func, args, VarCount::make_one())?;
                match dest {
                    ExprDestination::Register(dest) => {
                        assert_ne!(dest, source);
                        self.current_function()
                            .opcodes
                            .push(OpCode::Move { dest, source });
                        Some(dest)
                    }
                    ExprDestination::AllocateNew | ExprDestination::PushNew => {
                        assert_eq!(
                            self.current_function()
                                .register_allocator
                                .push(1)
                                .ok_or(CompilerError::Registers)?,
                            source
                        );
                        Some(source)
                    }
                    ExprDestination::None => None,
                }
            }

            ExprDescriptor::Comparison {
                mut left,
                op,
                mut right,
            } => {
                let left_reg_cons = self.expr_any_register_or_constant(&mut left)?;
                let right_reg_cons = self.expr_any_register_or_constant(&mut right)?;
                self.expr_discharge(*left, ExprDestination::None)?;
                self.expr_discharge(*right, ExprDestination::None)?;

                let dest = new_destination(self, dest)?;
                let comparison_opcode = comparison_binop_opcode(op, left_reg_cons, right_reg_cons);

                let opcodes = &mut self.current_function().opcodes;
                if let Some(dest) = dest {
                    opcodes.push(comparison_opcode);
                    opcodes.push(OpCode::Jump { offset: 1 });
                    opcodes.push(OpCode::LoadBool {
                        dest,
                        value: false,
                        skip_next: true,
                    });
                    opcodes.push(OpCode::LoadBool {
                        dest,
                        value: true,
                        skip_next: false,
                    });
                } else {
                    opcodes.push(comparison_opcode);
                    opcodes.push(OpCode::Jump { offset: 0 });
                }

                dest
            }

            ExprDescriptor::ShortCircuitBinOp {
                mut left,
                op,
                right,
            } => {
                let left_register = self.expr_any_register(&mut left)?;
                self.expr_discharge(*left, ExprDestination::None)?;
                let dest = new_destination(self, dest)?;

                let test_op_true = op == ShortCircuitBinOp::And;
                let test_op = if let Some(dest) = dest {
                    if left_register == dest {
                        OpCode::Test {
                            value: left_register,
                            is_true: test_op_true,
                        }
                    } else {
                        OpCode::TestSet {
                            dest,
                            value: left_register,
                            is_true: test_op_true,
                        }
                    }
                } else {
                    OpCode::Test {
                        value: left_register,
                        is_true: test_op_true,
                    }
                };
                self.current_function().opcodes.push(test_op);

                let jmp_inst = self.jump_placeholder();

                let right = self.expression(right)?;
                if let Some(dest) = dest {
                    self.expr_discharge(right, ExprDestination::Register(dest))?;
                } else {
                    self.expr_discharge(right, ExprDestination::None)?;
                }

                self.jump_here(jmp_inst)?;

                dest
            }
        };

        match dest {
            ExprDestination::Register(reg) => assert_eq!(result, Some(reg)),
            ExprDestination::AllocateNew => assert!(result.is_some()),
            ExprDestination::PushNew => {
                let r = result.unwrap().0;
                assert_eq!(
                    r as u16 + 1,
                    self.current_function().register_allocator.stack_top()
                );
            }
            ExprDestination::None => assert!(result.is_none()),
        }

        Ok(result)
    }

    // Performs a function call, consuming the func and args registers.  At the end of the function
    // call, the return values will be left at the top of the stack, and this method does not mark
    // the return registers as allocated.  Returns the register at which the returns (if any) are
    // placed (which will also be the current register allocator top).
    fn expr_function_call(
        &mut self,
        func: ExprDescriptor<'gc, 'a>,
        mut args: Vec<ExprDescriptor<'gc, 'a>>,
        returns: VarCount,
    ) -> Result<RegisterIndex, CompilerError> {
        let top_reg = self
            .expr_discharge(func, ExprDestination::PushNew)?
            .unwrap();
        let arg_count: u8 = cast(args.len()).ok_or(CompilerError::FixedParameters)?;
        let last_arg = args.pop();
        for arg in args {
            self.expr_discharge(arg, ExprDestination::PushNew)?;
        }

        if let Some(ExprDescriptor::FunctionCall { func, args }) = last_arg {
            self.expr_function_call(*func, args, VarCount::make_variable())?;
            self.current_function().opcodes.push(OpCode::Call {
                func: top_reg,
                args: VarCount::make_variable(),
                returns,
            });
        } else {
            if let Some(last_arg) = last_arg {
                self.expr_discharge(last_arg, ExprDestination::PushNew)?;
            }
            self.current_function().opcodes.push(OpCode::Call {
                func: top_reg,
                args: VarCount::make_constant(arg_count).ok_or(CompilerError::FixedParameters)?,
                returns,
            });
        }
        self.current_function()
            .register_allocator
            .pop_to(top_reg.0 as u16);

        Ok(top_reg)
    }
}

#[derive(Default)]
struct CompilerFunction<'gc, 'a> {
    constants: Vec<Value<'gc>>,
    constant_table: HashMap<ConstantValue<'gc>, ConstantIndex16>,

    upvalues: Vec<(&'a [u8], UpValueDescriptor)>,
    prototypes: Vec<FunctionProto<'gc>>,

    register_allocator: RegisterAllocator,

    has_varargs: bool,
    fixed_params: u8,
    locals: Vec<(&'a [u8], RegisterIndex)>,

    opcodes: Vec<OpCode>,
}

#[derive(Debug)]
enum VariableDescriptor<'a> {
    Local(RegisterIndex),
    UpValue(UpValueIndex),
    Global(&'a [u8]),
}

#[derive(Debug)]
enum ExprDescriptor<'gc, 'a> {
    Register {
        register: RegisterIndex,
        is_temporary: bool,
    },
    UpValue(UpValueIndex),
    Value(Value<'gc>),
    Not(Box<ExprDescriptor<'gc, 'a>>),
    FunctionCall {
        func: Box<ExprDescriptor<'gc, 'a>>,
        args: Vec<ExprDescriptor<'gc, 'a>>,
    },
    Comparison {
        left: Box<ExprDescriptor<'gc, 'a>>,
        op: ComparisonBinOp,
        right: Box<ExprDescriptor<'gc, 'a>>,
    },
    ShortCircuitBinOp {
        left: Box<ExprDescriptor<'gc, 'a>>,
        op: ShortCircuitBinOp,
        right: &'a Expression,
    },
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum ExprDestination {
    // Place the expression in the given previously allocated register
    Register(RegisterIndex),
    // Place the expression in a newly allocated register anywhere
    AllocateNew,
    // Place the expression in a newly allocated register at the top of the stack
    PushNew,
    // Evaluate the expression but do not place it anywhere
    None,
}