use cwe_checker_lib::{
    analysis::graph::{Graph, Node},
    intermediate_representation::{Arg, Blk, Def, Jmp, Sub, Term},
};
use log::{info, warn};
use petgraph::graph::NodeIndex;

use cwe_checker_lib::intermediate_representation::{ByteSize, Expression, Variable};

use cwe_checker_lib::intermediate_representation::Tid;

use crate::constraints::{
    ConstraintSet, DerivedTypeVar, Field, FieldLabel, SubtypeConstraint, TypeVariable,
    VariableManager,
};

use std::collections::{btree_set::BTreeSet, HashMap};

/// Gets a type variable for a [Tid] where multiple type variables need to exist at that [Tid] which are distinguished by which [Variable] they operate over.
pub fn tid_indexed_by_variable(tid: &Tid, var: &Variable) -> TypeVariable {
    TypeVariable::new(tid.get_str_repr().to_owned() + "_" + &var.name)
}

/// Converts a [Tid] to a [TypeVariable] by retrieving the string representation of the TID
pub fn tid_to_tvar(tid: &Tid) -> TypeVariable {
    TypeVariable::new(tid.get_str_repr().to_owned())
}

/// Converts a term to a TypeVariable by using its unique term identifier (Tid).
pub fn term_to_tvar<T>(term: &Term<T>) -> TypeVariable {
    tid_to_tvar(&term.tid)
}

/// Creates an actual argument type variable for the procedure
pub fn arg_tvar(index: usize, target_sub: &Tid) -> TypeVariable {
    TypeVariable::new(format!("arg_{}_{}", target_sub.get_str_repr(), index))
}

/// Maps a variable (register) to it's representing type variable at this time step in the program. This type variable is some representation of
/// all reaching definitions of this register.
pub trait RegisterMapping {
    /// Creates or returns the type variable representing this register at this program point. Takes a [VariableManager] so it
    /// can create fresh type variables.
    fn access(&self, var: &Variable, vman: &mut VariableManager) -> (TypeVariable, ConstraintSet);
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
/// Describes a [TypeVariable] for an abstract memory location where the acess may occur at some fixed offset into the type variable's cell.
pub struct TypeVariableAccess {
    /// The type variable which is accessed
    pub ty_var: TypeVariable,
    /// The size of the access
    pub sz: ByteSize,
    /// The potential constant offset at which the access occurs
    pub offset: Option<i64>,
}

/// Maps an address expression and a size to the possible type variables representing the loaded address at this program point.
/// Implementors of this trait effectively act as memory managers for the type inference algorithm.
pub trait PointsToMapping {
    /// Gets the set of type variables this address expression points to.  Takes a [VariableManager] so it
    /// can create fresh type variables.
    fn points_to(
        &self,
        address: &Expression,
        sz: ByteSize,
        vman: &mut VariableManager,
    ) -> BTreeSet<TypeVariableAccess>;
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
/// An arg can either be a memory access (generally passed via the stack), or directly avialable in a variable.
pub enum ArgTvar {
    /// Describes how to access the argument as a memory variable.
    MemTvar(TypeVariableAccess),
    /// Describes an argument in a register.
    VariableTvar(TypeVariable),
}

struct Memop {
    sz: ByteSize,
    addr: Expression,
    reg_value: TypeVariable,
    reg_constraints: ConstraintSet,
}

impl Memop {
    fn apply_mem_op(
        &self,
        points_to: &impl PointsToMapping,
        vman: &mut VariableManager,
        make_type_var: impl Fn(TypeVariableAccess) -> DerivedTypeVar,
        memop_is_upcasted: bool,
    ) -> ConstraintSet {
        let mut new_cons = self.reg_constraints.clone();
        let all_tvars = points_to.points_to(&self.addr, self.sz, vman);
        let all_dtvars: BTreeSet<DerivedTypeVar> =
            all_tvars.into_iter().map(|tv| make_type_var(tv)).collect();

        all_dtvars
            .into_iter()
            .map(|memop_tvar| {
                let reg_tvar = DerivedTypeVar::new(self.reg_value.clone());
                if memop_is_upcasted {
                    SubtypeConstraint::new(memop_tvar, reg_tvar)
                } else {
                    SubtypeConstraint::new(reg_tvar, memop_tvar)
                }
            })
            .for_each(|cons| {
                info!("{}", cons);
                new_cons.insert(cons);
            });
        ConstraintSet::from(new_cons)
    }
}

/// Links formal parameters with the type variable for their actual argument at callsites.
/// Additionally, links actual returns to formal returns at return sites.
pub trait SubprocedureLocators {
    // TODO(ian) may need the subprocedure tid here.
    /// Takes a points-to and register mapping to resolve type variables of inputs and outputs
    fn get_type_variables_and_constraints_for_arg(
        &self,
        arg: &Arg,
        reg: &impl RegisterMapping,
        points_to: &impl PointsToMapping,
        vm: &mut VariableManager,
    ) -> (BTreeSet<ArgTvar>, ConstraintSet);
}

/// Represents the flow-sensitive context needed by flow-insensitive constraint generation to generate type variables and constraints at a given program point.
/// The register mapping provides constraints and type variables to represent a register when it is accessed via some notion of reaching definitions.
/// The PointsToMapping determines the set of a type variables a load or store points to in order to generate constraints.
/// Finally the SubprocedureLocators are used to link actual and formal arguments and returns within constraints.
pub struct NodeContext<R: RegisterMapping, P: PointsToMapping, S: SubprocedureLocators> {
    reg_map: R,
    points_to: P,
    subprocedure_locators: S,
}

impl<R: RegisterMapping, P: PointsToMapping, S: SubprocedureLocators> NodeContext<R, P, S> {
    /// Given a register, pointer, and subprocedure mapping, generates a full NodeContext.
    pub fn new(r: R, p: P, s: S) -> NodeContext<R, P, S> {
        NodeContext {
            reg_map: r,
            points_to: p,
            subprocedure_locators: s,
        }
    }

    fn evaluate_expression(
        &self,
        value: &Expression,
        vman: &mut VariableManager,
    ) -> (TypeVariable, ConstraintSet) {
        match &value {
            Expression::Var(v2) => {
                let (rhs_type_var, additional_constraints) = self.reg_map.access(v2, vman);
                (rhs_type_var, additional_constraints)
            }
            _ => {
                warn!("Unhandled expression: {:?}", value);
                (vman.fresh(), ConstraintSet::empty())
            } // TODO(ian) handle additional constraints, add/sub
        }
    }

    fn generate_expression_constraint(
        &self,
        lhs_type_var: TypeVariable,
        value: &Expression,
        vman: &mut VariableManager,
    ) -> ConstraintSet {
        let (rhs_type_var, mut constraints) = self.evaluate_expression(value, vman);
        constraints.insert(SubtypeConstraint::new(
            DerivedTypeVar::new(rhs_type_var),
            DerivedTypeVar::new(lhs_type_var),
        ));
        constraints
    }

    fn apply_assign(
        &self,
        tid: &Tid,
        var: &Variable,
        value: &Expression,
        vman: &mut VariableManager,
    ) -> ConstraintSet {
        let mut constraints = ConstraintSet::default();
        let typ_var = tid_indexed_by_variable(tid, var);
        let new_cons = self.generate_expression_constraint(typ_var, value, vman);
        constraints.insert_all(&new_cons);
        constraints
    }

    fn make_mem_tvar(var: TypeVariableAccess, label: FieldLabel) -> DerivedTypeVar {
        let mut der_var = DerivedTypeVar::new(var.ty_var);
        der_var.add_field_label(label);
        if let Some(off) = var.offset {
            der_var.add_field_label(FieldLabel::Field(Field::new(off, var.sz.as_bit_length())));
        }
        der_var
    }
    fn make_loaded_tvar(var: TypeVariableAccess) -> DerivedTypeVar {
        Self::make_mem_tvar(var, FieldLabel::Load)
    }

    fn apply_load(
        &self,
        tid: &Tid,
        v_into: &Variable,
        address: &Expression,
        vman: &mut VariableManager,
    ) -> ConstraintSet {
        let constraints = ConstraintSet::default();
        let typ_var = tid_indexed_by_variable(tid, v_into);
        let memop = Memop {
            sz: v_into.size,
            addr: address.clone(),
            reg_value: typ_var,
            reg_constraints: constraints,
        };
        memop.apply_mem_op(&self.points_to, vman, Self::make_loaded_tvar, true)
    }

    fn make_store_tvar(var: TypeVariableAccess) -> DerivedTypeVar {
        Self::make_mem_tvar(var, FieldLabel::Store)
    }

    fn apply_store(
        &self,
        tid: &Tid,
        value_from: &Expression,
        address_into: &Expression,
        vman: &mut VariableManager,
    ) -> ConstraintSet {
        info!("{}: store", tid);
        let (reg_val, constraints) = self.evaluate_expression(value_from, vman);
        info!("{}: store {}", tid, reg_val);

        let memop = Memop {
            sz: value_from.bytesize(),
            addr: address_into.clone(),
            reg_value: reg_val,
            reg_constraints: constraints,
        };
        memop.apply_mem_op(&self.points_to, vman, Self::make_store_tvar, false)
    }

    fn handle_def(&self, df: &Term<Def>, vman: &mut VariableManager) -> ConstraintSet {
        match &df.term {
            Def::Load { var, address } => self.apply_load(&df.tid, var, address, vman),
            Def::Store { address, value } => self.apply_store(&df.tid, value, address, vman),
            Def::Assign { var, value } => self.apply_assign(&df.tid, var, value, vman),
        }
    }

    fn handle_block_start(&self, blk: &Term<Blk>, vman: &mut VariableManager) -> ConstraintSet {
        blk.term
            .defs
            .iter()
            .map(|df: &Term<Def>| self.handle_def(df, vman))
            .fold(ConstraintSet::default(), |x, y| {
                ConstraintSet::from(
                    x.union(&y)
                        .cloned()
                        .collect::<BTreeSet<SubtypeConstraint>>(),
                )
            })
    }

    fn argtvar_to_dtv(tvar: ArgTvar) -> DerivedTypeVar {
        match tvar {
            ArgTvar::VariableTvar(tv) => DerivedTypeVar::new(tv),
            ArgTvar::MemTvar(tv_access) => Self::make_loaded_tvar(tv_access),
        }
    }

    fn arg_to_constraints(
        &self,
        arg: &Arg,
        formal: &DerivedTypeVar,
        vm: &mut VariableManager,
        arg_is_subtype_of_representations: bool,
    ) -> ConstraintSet {
        let (type_vars_for_arg, mut additional_constraints) = self
            .subprocedure_locators
            .get_type_variables_and_constraints_for_arg(arg, &self.reg_map, &self.points_to, vm);
        type_vars_for_arg
            .into_iter()
            .map(|tvar| {
                if arg_is_subtype_of_representations {
                    SubtypeConstraint::new(formal.clone(), Self::argtvar_to_dtv(tvar))
                } else {
                    SubtypeConstraint::new(Self::argtvar_to_dtv(tvar), formal.clone())
                }
            })
            .for_each(|cons| {
                additional_constraints.insert(cons);
            });
        additional_constraints
    }

    fn create_actual_args(sz: usize, target_func: &Term<Sub>) -> Vec<DerivedTypeVar> {
        (0..sz)
            .map(|idx| DerivedTypeVar::new(arg_tvar(idx, &target_func.tid)))
            .collect()
    }

    fn handle_call(&self, target_function: &Term<Sub>, vm: &mut VariableManager) -> ConstraintSet {
        self.handle_function_args(
            target_function,
            &Self::create_actual_args(target_function.term.formal_args.len(), target_function),
            &target_function.term.formal_args,
            vm,
            &|ind| FieldLabel::In(ind),
            false,
        )
    }

    /// The formal is always a subtype of the actual (this is a mangling of the actual/formal term)
    fn generate_formal_constraint(
        &self,
        formal: DerivedTypeVar,
        actual: DerivedTypeVar,
    ) -> SubtypeConstraint {
        SubtypeConstraint::new(formal, actual)
    }

    fn handle_function_args(
        &self,
        target_function: &Term<Sub>,
        actual_typevars: &[DerivedTypeVar],
        args: &[Arg],
        vm: &mut VariableManager,
        index_to_field_label: &impl Fn(usize) -> FieldLabel,
        arg_is_subtype_of_representations: bool,
    ) -> ConstraintSet {
        assert_eq!(args.len(), actual_typevars.len());
        let mut constraints = ConstraintSet::default();

        args.iter()
            .enumerate()
            .map(|(ind, arg)| {
                let mut formal = DerivedTypeVar::new(term_to_tvar(target_function));
                formal.add_field_label(index_to_field_label(ind));
                let actual = &actual_typevars[ind];
                let mut cons =
                    self.arg_to_constraints(arg, &formal, vm, arg_is_subtype_of_representations);

                cons.insert(self.generate_formal_constraint(formal, actual.clone()));
                // cons.insert_all(self.)
                cons
            })
            .for_each(|cons| constraints.insert_all(&cons));
        constraints
    }

    fn create_actual_rets(call: &Term<Jmp>, rets: &[Arg]) -> Vec<DerivedTypeVar> {
        rets.iter()
            .map(|arg| match arg {
                Arg::Register {
                    var,
                    data_type: _data_type,
                } => DerivedTypeVar::new(tid_indexed_by_variable(&call.tid, var)),
                // TODO(ian): handle this with anyhow
                Arg::Stack { .. } => panic!("Cannot handle return value on the stack"),
            })
            .collect()
    }

    fn handle_return(
        &self,
        call: &Term<Jmp>,
        return_from: &Term<Sub>,
        vm: &mut VariableManager,
    ) -> ConstraintSet {
        let act_rets = Self::create_actual_rets(call, &return_from.term.formal_rets);
        self.handle_function_args(
            return_from,
            &act_rets,
            &return_from.term.formal_rets,
            vm,
            &|ind| FieldLabel::Out(ind),
            false,
        )
    }
}

/// Holds a mapping between the nodes and their flow-sensitive analysis results, which
/// are needed for constraint generation
pub struct Context<'a, R, P, S>
where
    R: RegisterMapping,
    P: PointsToMapping,
    S: SubprocedureLocators,
{
    graph: &'a Graph<'a>,
    node_contexts: HashMap<NodeIndex, NodeContext<R, P, S>>,
}

impl<'a, R, P, S> Context<'a, R, P, S>
where
    R: RegisterMapping,
    P: PointsToMapping,
    S: SubprocedureLocators,
{
    /// Creates a new context for type constraint generation.
    pub fn new(
        graph: &'a Graph<'a>,
        node_contexts: HashMap<NodeIndex, NodeContext<R, P, S>>,
    ) -> Context<'a, R, P, S> {
        Context {
            graph,
            node_contexts,
        }
    }

    fn call_blk_to_call<'b>(
        calling_blk: &'b Term<Blk>,
        target_func: &Term<Sub>,
    ) -> Option<&'b Term<Jmp>> {
        calling_blk.term.jmps.iter().find(|jmp| match &jmp.term {
            Jmp::Call {
                target: tid,
                return_: _return,
            } => *tid == target_func.tid,
            _ => false,
        })
    }

    fn generate_constraints_for_node(
        &self,
        nd_ind: NodeIndex,
        vman: &mut VariableManager,
    ) -> ConstraintSet {
        let nd_cont = self.node_contexts.get(&nd_ind);
        let nd = self.graph[nd_ind];

        if let Some(nd_cont) = nd_cont {
            match nd {
                Node::BlkStart(blk, _sub) => nd_cont.handle_block_start(blk, vman),
                Node::CallReturn {
                    call: (call_blk, _calling_proc),
                    return_: (_returned_to_blk, return_proc),
                } => nd_cont.handle_return(
                    Self::call_blk_to_call(call_blk, return_proc).expect(
                        "Invalid CFG where calling blk does not contain returning function.",
                    ),
                    return_proc,
                    vman,
                ),
                Node::CallSource {
                    source: _source,
                    target: (_calling_blk, target_func),
                } => nd_cont.handle_call(target_func, vman),
                // block post conditions arent needed to generate constraints
                Node::BlkEnd(_blk, _term) => Default::default(),
            }
        } else {
            ConstraintSet::default()
        }
    }

    /// Walks all of the nodes and gather the inferred subtyping constraints.
    pub fn generate_constraints(&self) -> ConstraintSet {
        let mut vman = VariableManager::new();
        let mut cs: ConstraintSet = Default::default();
        for nd_ind in self.graph.node_indices() {
            cs = ConstraintSet::from(
                cs.union(&self.generate_constraints_for_node(nd_ind, &mut vman))
                    .cloned()
                    .collect::<BTreeSet<SubtypeConstraint>>(),
            );
        }
        cs
    }
}
