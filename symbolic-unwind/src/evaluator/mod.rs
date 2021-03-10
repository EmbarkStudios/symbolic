//! Functionality for evaluating *Breakpad
//! [RPN](https://en.wikipedia.org/wiki/Reverse_Polish_notation) expressions*.
//!
//! These expressions are defined by the following
//! [BNF](https://en.wikipedia.org/wiki/Backus%E2%80%93Naur_form) specification:
//! ```text
//! <rule>       ::=  <register>: <expr>
//! <assignment> ::=  <variable> <expr> =
//! <expr>       ::=  <register> | <literal> | <expr> <expr> <binop> | <expr> ^
//! <register>   ::=  <constant> | <variable>
//! <constant>   ::=  [a-zA-Z_.][a-zA-Z0-9_.]*
//! <variable>   ::=  $[a-zA-Z][a-zA-Z0-9]*
//! <binop>      ::=  + | - | * | / | % | @
//! <literal>    ::=  [0-9a-fA-F]+
//! ```
//! Most of this syntax should be familiar. The symbol `^` denotes a dereference operation,
//! i.e. assuming that some representation `m` of a region of memory is available,
//! `x ^` evaluates to `m[x]`. If no memory is available or `m` is not defined at `x`,
//! evaluating the expression will fail. The symbol
//! `@` denotes an align operation; it truncates its first operand to a multiple of its
//! second operand.
//!
//! Registers are evaluated by referring to a dictionary
//! (concretely: a [`BTreeMap`]). If an expression contains a register that is
//! not in the dictionary, evaluating the expression will fail.
//!
//! # Assignments and rules
//!
//! Breakpad `STACK WIN` records (see [here](https://github.com/google/breakpad/blob/main/docs/symbol_files.md#stack-win-records)
//! can contain `program strings` that describe how to compute the values of registers
//! in an earlier call frame. These program strings are effectively sequences of the
//! assignments described above. They can be be parsed with the
//! [assignment](parsing::assignment), [assignment_complete](parsing::assignment_complete),
//! [assignments](parsing::assignments),
//! and [assignments_complete](parsing::assignments_complete) parsers.
//!
//! By contrast, Breakpad `STACK CFI` records (see [here](https://github.com/google/breakpad/blob/main/docs/symbol_files.md#stack-cfi-records)
//! contain sequences of rules for essentially the same purpose. They can be parsed with the
//! [rule](parsing::rule), [rule_complete](parsing::rule_complete),
//! [rules](parsing::rules),
//! and [rules_complete](parsing::rules_complete) parsers.
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

use super::base::{Endianness, MemoryRegion, RegisterValue};
use parsing::ParseExprError;

pub mod parsing;

/// Structure that encapsulates the information necessary to evaluate Breakpad
/// RPN expressions.
///
/// It is generic over:
/// - An address type, which is used both for basic expressions and for pointers into `memory`
/// - An [`Endianness`](super::base::Endianness) that controls how values are read from memory
pub struct Evaluator<'memory, A, E> {
    /// A region of memory.
    ///
    /// If this is `None`, evaluation of expressions containing dereference
    /// operations will fail.
    memory: Option<MemoryRegion<'memory>>,

    /// A map containing the values of registers.
    ///
    /// Trying to use a register that is not in this map will cause evaluation to fail.
    /// This map can be modified by the [`assign`](Self::assign) and
    ///  [`process`](Self::process) methods.
    registers: BTreeMap<Register, A>,

    /// The endianness the evaluator uses to read data from memory.
    endian: E,

    /// A cache for values of registers computed by [`evaluate_register`](Self::evaluate_register)
    /// and [`evaluate_all_registers`](Self::evaluate_all_registers).
    register_cache: BTreeMap<Register, A>,

    /// A map from registers to expressions that describes how to compute the "new"
    /// (in stackwalking terms: the caller's) values of registers from the current ones.
    cfi_rules: BTreeMap<Register, Expr<A>>,
}

impl<'memory, A, E> Evaluator<'memory, A, E> {
    /// Creates an Evaluator with the given endianness, no memory, and empty
    /// constant and variable maps.
    pub fn new(endian: E) -> Self {
        Self {
            memory: None,
            registers: BTreeMap::new(),
            endian,
            register_cache: BTreeMap::new(),
            cfi_rules: BTreeMap::new(),
        }
    }

    /// Sets the evaluator's memory to the given `MemoryRegion`.
    pub fn memory(mut self, memory: MemoryRegion<'memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Sets the evaluator's register map to the given map.
    pub fn registers(mut self, registers: BTreeMap<Register, A>) -> Self {
        self.registers = registers;
        self
    }

    /// Add a new register rule to the evaluator. These rules are used by
    /// [`evaluate_register`](Self::evaluate_register)
    /// and [`evaluate_all_registers`](Self::evaluate_all_registers).
    pub fn add_cfi_rule(&mut self, register: Register, expr: Expr<A>) {
        self.cfi_rules.insert(register, expr);
    }
}

impl<'memory, A: RegisterValue, E: Endianness> Evaluator<'memory, A, E> {
    /// Evaluates a single expression.
    ///
    /// This may fail if the expression tries to dereference unavailable memory
    /// or uses undefined registers.
    pub fn evaluate(&self, expr: &Expr<A>) -> Result<A, EvaluationError> {
        let Evaluator {
            memory,
            registers,
            endian,
            ..
        } = self;
        Self::evaluate_inner(expr, &registers, &memory, *endian)
    }

    /// Evaluates the given register's rule and returns the value.
    ///
    /// This fails if there is no rule for the register or it cannot be evaluated.
    /// Results are cached.
    pub fn evaluate_register(&mut self, register: &Register) -> Result<A, EvaluationError> {
        let Evaluator {
            ref mut registers,
            ref mut register_cache,
            cfi_rules,
            memory,
            endian,
        } = self;
        Self::evaluate_register_inner(
            register,
            cfi_rules,
            registers,
            register_cache,
            &memory,
            *endian,
        )
    }

    /// Evaluates all register rules and returns the results in a map.
    ///
    /// This fails if one of the rules cannot be evaluated. Results are cached.
    pub fn evaluate_all_registers(&mut self) -> Result<BTreeMap<Register, A>, EvaluationError> {
        let Evaluator {
            ref mut registers,
            ref mut register_cache,
            memory,
            cfi_rules,
            endian,
        } = self;
        cfi_rules
            .keys()
            .cloned()
            .map(|r| {
                Self::evaluate_register_inner(
                    &r,
                    &cfi_rules,
                    registers,
                    register_cache,
                    &memory,
                    *endian,
                )
                .map(|v| (r, v))
            })
            .collect()
    }

    /// Processes a string of rules and outputs a new map of register values.
    ///
    /// The processing follows Breakpad's rules: if the rule for a register `foo`
    /// mentions other registers `bar`, `baz`, that means that the new value of `foo`
    /// will be computed from the *current* values of `bar` and `baz`. There is one
    /// exception, however: rules may refer to the `CFA` register that itself
    /// needs to be computed from a rule. As a consequence, it
    /// doesn't matter in which order rules are evaluated, apart from the fact that
    /// the rule for `CFA` must be evaluated first.
    ///
    /// # Example
    /// ```
    /// use std::collections::BTreeMap;
    /// use symbolic_unwind::evaluator::{Evaluator, Register};
    /// use symbolic_unwind::BigEndian;
    /// let input = ".cfa: $r0 4 - $r0: 3 $r1: .cfa $r0 +";
    /// let mut registers = BTreeMap::new();
    /// let r0 = "$r0".parse::<Register>().unwrap();
    /// let r1 = "$r1".parse::<Register>().unwrap();
    /// registers.insert(r0.clone(), 17u8);
    /// let mut evaluator = Evaluator::new(BigEndian).registers(registers);
    ///
    /// // Currently, evaluator.registers == { $r0: 17 }
    /// let new_registers = evaluator.process_rules(input).unwrap();
    ///
    /// // The calculation of $r1 used the newly computed value of .cfa, but the old
    /// // value of r0.
    /// assert_eq!(
    ///     new_registers,
    ///     vec![(Register::cfa(), 13), (r0, 3), (r1, 30)]
    ///         .into_iter()
    ///         .collect()
    /// );
    /// ```
    pub fn process_rules(&mut self, input: &str) -> Result<BTreeMap<Register, A>, ExpressionError> {
        parsing::rules_complete(input)?
            .into_iter()
            .for_each(|Rule(r, v)| self.add_cfi_rule(r, v));

        Ok(self.evaluate_all_registers()?)
    }

    /// Evaluates a single expression.
    ///
    /// This function is used internally by [`evaluate`](Self::evaluate).
    /// It takes the fields it requires as individual arguments for finer-grained borrow
    /// checking.
    /// It may fail if the expression tries to dereference unavailable memory
    /// or uses undefined registers.
    fn evaluate_inner(
        expr: &Expr<A>,
        registers: &BTreeMap<Register, A>,
        memory: &Option<MemoryRegion>,
        endian: E,
    ) -> Result<A, EvaluationError> {
        use Expr::*;

        let val = match expr {
            Value(x) => *x,
            Reg(i) => {
                if let Some(val) = registers.get(&i) {
                    *val
                } else {
                    return Err(EvaluationError(EvaluationErrorInner::UndefinedRegister(
                        i.clone(),
                    )));
                }
            }
            Op(e1, e2, op) => {
                let e1 = Self::evaluate_inner(&*e1, registers, memory, endian)?;
                let e2 = Self::evaluate_inner(&*e2, registers, memory, endian)?;
                match op {
                    BinOp::Add => e1 + e2,
                    BinOp::Sub => e1 - e2,
                    BinOp::Mul => e1 * e2,
                    BinOp::Div => e1 / e2,
                    BinOp::Mod => e1 % e2,
                    BinOp::Align => e2 * (e1 / e2),
                }
            }

            Deref(address) => {
                let address = Self::evaluate_inner(&*address, registers, memory, endian)?;
                let memory =
                    memory.ok_or(EvaluationError(EvaluationErrorInner::MemoryUnavailable))?;
                if let Some(val) = memory.get(address, endian) {
                    val
                } else {
                    return Err(EvaluationError(EvaluationErrorInner::IllegalMemoryAccess {
                        address: address.try_into().ok(),
                        bytes: A::WIDTH,
                        address_range: memory.base_addr..memory.base_addr + memory.len() as u64,
                    }));
                }
            }
        };
        Ok(val)
    }

    /// Evaluates the given register's rule and returns the value.
    ///
    /// This function is used internally by both
    /// [`evaluate_register`](Self::evaluate_register)
    /// and [`evaluate_all_registers`](Self::evaluate_all_registers). It takes
    /// the fields it requires as individual arguments for finer-grained borrow checking.
    ///
    /// This fails if there is no rule for the register or it cannot be evaluated.
    /// Results are cached.
    fn evaluate_register_inner(
        register: &Register,
        cfi_rules: &BTreeMap<Register, Expr<A>>,
        registers: &mut BTreeMap<Register, A>,
        register_cache: &mut BTreeMap<Register, A>,
        memory: &Option<MemoryRegion>,
        endian: E,
    ) -> Result<A, EvaluationError> {
        if let Some(val) = register_cache.get(register) {
            return Ok(*val);
        }

        let rule = match cfi_rules.get(register) {
            Some(expr) => expr,
            None => {
                return Err(EvaluationError(EvaluationErrorInner::NoRuleForRegister(
                    register.clone(),
                )))
            }
        };

        // Calculate cfa first if necessary.
        let cfa = Register::cfa();
        if rule.contains_cfa() && !registers.contains_key(&cfa) {
            let cfa_val = Self::evaluate_register_inner(
                &cfa,
                cfi_rules,
                registers,
                register_cache,
                memory,
                endian,
            )?;
            registers.insert(cfa, cfa_val);
        }

        let val = Self::evaluate_inner(&rule, registers, memory, endian)?;
        register_cache.insert(register.clone(), val);
        Ok(val)
    }
}

/// An error encountered while evaluating an expression.
#[derive(Debug)]
#[non_exhaustive]
enum EvaluationErrorInner {
    /// The expression contains an undefined register name.
    UndefinedRegister(Register),

    /// Tried to evaluate a register for which no rule exists.
    NoRuleForRegister(Register),

    /// The expression contains a dereference, but the evaluator does not have access
    /// to any memory.
    MemoryUnavailable,

    /// The requested piece of memory would exceed the bounds of the memory region.
    IllegalMemoryAccess {
        /// The number of bytes that were tried to read.
        bytes: usize,
        /// The address at which the read was attempted.
        address: Option<usize>,
        /// The range of available addresses.
        address_range: std::ops::Range<u64>,
    },
}

impl fmt::Display for EvaluationErrorInner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
           Self::UndefinedRegister(r) => write!(f, "Register {} is not defined", r),
           Self::NoRuleForRegister(r) => write!(f, "There is no rule for evaluating register {}", r),
           Self::MemoryUnavailable => write!(f, "The evaluator does not have access to memory"),
           Self::IllegalMemoryAccess {
               bytes, address: Some(address), address_range
           } => write!(f, "Tried to read {} bytes at memory address {}. The available address range is [{}, {})", bytes, address, address_range.start, address_range.end),
        Self::IllegalMemoryAccess {
            bytes, address: None, ..
        } => write!(f, "Tried to read {} bytes at address that exceeds the maximum usize value", bytes),
        }
    }
}

/// An error encountered while evaluating an expression.
#[derive(Debug)]
pub struct EvaluationError(EvaluationErrorInner);

impl fmt::Display for EvaluationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for EvaluationError {}

/// An error encountered while parsing or evaluating an expression.
#[derive(Debug)]
enum ExpressionErrorInner {
    /// An error was encountered while parsing an expression.
    Parsing(ParseExprError),

    /// An error was encountered while evaluating an expression.
    Evaluation(EvaluationError),
}

impl From<ParseExprError> for ExpressionError {
    fn from(other: ParseExprError) -> Self {
        Self(ExpressionErrorInner::Parsing(other))
    }
}

impl From<EvaluationError> for ExpressionError {
    fn from(other: EvaluationError) -> Self {
        Self(ExpressionErrorInner::Evaluation(other))
    }
}

impl fmt::Display for ExpressionErrorInner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Parsing(e) => write!(f, "Error while parsing: {}", e),
            Self::Evaluation(e) => write!(f, "Error while evaluating: {}", e),
        }
    }
}

/// An error encountered while parsing or evaluating an expression.
#[derive(Debug)]
pub struct ExpressionError(ExpressionErrorInner);

impl fmt::Display for ExpressionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for ExpressionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self.0 {
            ExpressionErrorInner::Parsing(ref e) => Some(e),
            ExpressionErrorInner::Evaluation(ref e) => Some(e),
        }
    }
}

/// A register.
///
/// Registers come in two flavors: "constants" and "variables". They can be told
/// apart by the fact that the names of variables begin with the symbol `$`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Register {
    /// The CFA (Canonical Frame Address) register.
    Cfa,

    /// A variable.
    Var(String),

    /// A constant.
    Const(String),
}

impl Register {
    /// Returns the `CFA` (Canonical Frame Address) register, usually called `.cfa`.
    pub fn cfa() -> Self {
        Self::Cfa
    }

    /// Returns the `RA` (Return Address) register, usually called `.ra`.
    pub fn ra() -> Self {
        Self::Const(".ra".to_string())
    }

    /// Returns true if this is a variable register, that is, if its name begins with "`$`".
    pub fn is_variable(&self) -> bool {
        match self {
            Self::Cfa => false,
            Self::Var(_) => true,
            Self::Const(_) => false,
        }
    }

    /// Returns true if this is a constant register, that is, if its name dooes not begin with "`$`".
    pub fn is_constant(&self) -> bool {
        match self {
            Self::Cfa => true,
            Self::Const(_) => true,
            Self::Var(_) => false,
        }
    }

    /// Returns true if this is the CFA register.
    pub fn is_cfa(&self) -> bool {
        matches!(self, Self::Cfa)
    }
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Cfa => ".cfa".fmt(f),
            Self::Var(v) => v.fmt(f),
            Self::Const(c) => c.fmt(f),
        }
    }
}

impl FromStr for Register {
    type Err = ParseExprError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        parsing::register_complete(input)
    }
}

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub enum BinOp {
    /// Addition.
    Add,

    /// Subtraction.
    Sub,

    /// Multiplication.
    Mul,

    /// Division.
    Div,

    /// Remainder.
    Mod,

    /// Alignment.
    ///
    /// Truncates the first operand to a multiple of the second operand.
    Align,
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Add => write!(f, "+"),
            Self::Sub => write!(f, "-"),
            Self::Mul => write!(f, "*"),
            Self::Div => write!(f, "/"),
            Self::Mod => write!(f, "%"),
            Self::Align => write!(f, "@"),
        }
    }
}

/// An expression.
///
/// This is generic so that different number types can be used.
#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub enum Expr<T> {
    /// A base value.
    Value(T),

    /// A register name.
    Reg(Register),

    /// An expression `a b §`, where `§` is a [binary operator](BinOp).
    Op(Box<Expr<T>>, Box<Expr<T>>, BinOp),

    /// A dereferenced subexpression.
    Deref(Box<Expr<T>>),
}

impl<T> Expr<T> {
    /// Returns true if the expression contains the CFA register.
    fn contains_cfa(&self) -> bool {
        match self {
            Self::Value(_) => false,
            Self::Reg(r) => r.is_cfa(),
            Self::Op(e1, e2, _) => e1.contains_cfa() || e2.contains_cfa(),
            Self::Deref(e) => e.contains_cfa(),
        }
    }
}

impl<T: fmt::Display> fmt::Display for Expr<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Value(n) => write!(f, "{}", n),
            Self::Reg(i) => write!(f, "{}", i),
            Self::Op(x, y, op) => write!(f, "{} {} {}", x, y, op),
            Self::Deref(x) => write!(f, "{} ^", x),
        }
    }
}

impl<T: RegisterValue> FromStr for Expr<T> {
    type Err = ParseExprError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        parsing::expr_complete(input)
    }
}

/// An assignment `v e =` where `v` is a [variable register](Register) and
/// `e` is an [expression](Expr).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment<T>(Register, Expr<T>);

impl<T> Assignment<T> {
    /// Creates a new assignment.
    ///
    /// This will fail if `variable` is not a variable register.
    pub fn new(variable: Register, expr: Expr<T>) -> Option<Self> {
        variable.is_variable().then(|| Self(variable, expr))
    }
}

impl<T: fmt::Display> fmt::Display for Assignment<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} {} =", self.0, self.1)
    }
}

impl<T: RegisterValue> FromStr for Assignment<T> {
    type Err = ParseExprError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        parsing::assignment_complete(input)
    }
}

/// A `STACK CFI` rule `reg: e`, where `reg` is a [register](Register) and `e` is an expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule<A: RegisterValue>(pub Register, pub Expr<A>);

impl<A: RegisterValue + fmt::Display> fmt::Display for Rule<A> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.0, self.1)
    }
}

/// These tests are inspired by the Breakpad PostfixEvaluator unit tests:
/// [https://github.com/google/breakpad/blob/main/src/processor/postfix_evaluator_unittest.cc]
#[cfg(test)]
mod test {
    use super::*;
    use crate::base::BigEndian;

    #[test]
    fn test_cfa() {
        assert!(".cfa".parse::<Register>().unwrap().is_cfa());
    }

    #[test]
    fn test_rules() {
        let input = "$rAdd3: 2 2 + $rMul2: 9 6 *";

        let mut eval = Evaluator::<u64, _>::new(BigEndian);
        let r_add3 = "$rAdd3".parse::<Register>().unwrap();
        let r_mul2 = "$rMul2".parse::<Register>().unwrap();

        let new_registers = eval.process_rules(input).unwrap();

        assert_eq!(
            new_registers,
            vec![(r_add3, 4), (r_mul2, 54)].into_iter().collect()
        );
    }

    #[test]
    fn test_deref() {
        let input = "$rDeref: 9 ^";

        let memory = MemoryRegion {
            base_addr: 9,
            contents: &[0, 0, 0, 0, 0, 0, 0, 10],
        };

        let mut eval = Evaluator::<u64, _>::new(BigEndian).memory(memory);

        let r_deref = "$rDeref".parse::<Register>().unwrap();

        let new_registers = eval.process_rules(input).unwrap();

        assert_eq!(new_registers, vec![(r_deref, 10)].into_iter().collect());
    }

    #[test]
    fn single_register() {
        let sp = ".sp".parse::<Register>().unwrap();
        let r0 = "$r0".parse::<Register>().unwrap();
        let cfa = Register::cfa();

        let registers = vec![(sp.clone(), 17), (r0.clone(), 5)]
            .into_iter()
            .collect();

        let mut evaluator = Evaluator::<u32, _>::new(BigEndian).registers(registers);
        evaluator.add_cfi_rule(cfa.clone(), "$r0 8 +".parse().unwrap());
        evaluator.add_cfi_rule(sp.clone(), ".cfa a %".parse().unwrap());

        assert_eq!(evaluator.evaluate_register(&sp).unwrap(), 3);
        assert_eq!(evaluator.evaluate_register(&cfa).unwrap(), 0xd);
        assert!(evaluator.evaluate_register(&r0).is_err());
    }

    #[test]
    fn all_registers() {
        let sp = ".sp".parse::<Register>().unwrap();
        let r0 = "$r0".parse::<Register>().unwrap();
        let cfa = Register::cfa();

        let registers = vec![(sp.clone(), 17), (r0.clone(), 5)]
            .into_iter()
            .collect();

        let mut evaluator = Evaluator::<u32, _>::new(BigEndian).registers(registers);
        evaluator.add_cfi_rule(cfa.clone(), "$r0 8 +".parse().unwrap());
        evaluator.add_cfi_rule(sp.clone(), ".cfa a %".parse().unwrap());

        let result = evaluator.evaluate_all_registers().unwrap();

        assert_eq!(result[&sp], 3);
        assert_eq!(result[&cfa], 0xd);
        assert!(!result.contains_key(&r0));
    }
}
