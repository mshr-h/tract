use crate::internal::*;
use std::fmt;

clone_trait_object!(BinMiniOp);
pub trait BinMiniOp: fmt::Debug + objekt::Clone + Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn operating_datum_type(&self, a: DatumType, b: DatumType) -> TractResult<DatumType>;
    fn result_datum_type(&self, a: DatumType, b: DatumType) -> TractResult<DatumType>;
    fn eval_out_of_place(&self, c: &mut Tensor, a: &Tensor, b: &Tensor) -> TractResult<()>;
    fn eval_broadcast_and_typecast(
        &self,
        mut inputs: TVec<Arc<Tensor>>,
    ) -> TractResult<TVec<Arc<Tensor>>> {
        let (a, b) = args_2!(inputs);
        let op_type = self.operating_datum_type(a.datum_type(), b.datum_type())?;
        let a = a.cast_to_dt(op_type)?.into_owned();
        let b = b.cast_to_dt(op_type)?.into_owned();
        self.eval_broadcast(tvec!(a.into_arc_tensor(), b.into_arc_tensor()))
    }
    fn eval_broadcast(&self, mut inputs: TVec<Arc<Tensor>>) -> TractResult<TVec<Arc<Tensor>>> {
        let (a, b) = args_2!(inputs);
        let c_shape = crate::broadcast::multi_broadcast(&[a.shape(), b.shape()])
            .ok_or("Can not compute resulting shape")?;
        let c_dt = self.result_datum_type(a.datum_type(), b.datum_type())?;
        let mut c = unsafe { Tensor::uninitialized_dt(c_dt, &*c_shape)? };
        self.eval_out_of_place(&mut c, a.as_ref(), b.as_ref())?;
        Ok(tvec!(c.into_arc_tensor()))
    }
}

#[derive(Debug, Clone)]
pub struct InferenceBinOp(pub Box<dyn BinMiniOp>);

impl Op for InferenceBinOp {
    fn name(&self) -> Cow<str> {
        format!("{}Inference", self.0.name()).into()
    }

    fn declutter(
        &self,
        model: &TypedModel,
        node: &TypedNode,
    ) -> TractResult<Option<TypedModelPatch>> {
        let facts = model.node_input_facts(node.id)?;
        let mut patch = TypedModelPatch::default();
        let operating_datum_type = self.0.operating_datum_type(facts[0].datum_type, facts[1].datum_type)?;
        let bin = patch.add_node(
            &*node.name,
            TypedBinOp(self.0.clone()),
            tvec!(node.outputs[0].fact.clone()),
        )?;
        patch.shunt_outside(OutletId::new(node.id, 0), OutletId::new(bin, 0))?;
        for i in 0..2 {
            let fact = model.node_input_facts(node.id)?[i];
            let tap = patch.tap_model(model, node.inputs[i])?;
            if fact.datum_type != operating_datum_type {
                let mut fact = fact.clone();
                fact.datum_type = operating_datum_type;
                let cast = patch.chain_after(
                    tap,
                    format!("{}Cast{}", &*node.name, i),
                    super::cast::Cast::new(operating_datum_type),
                    tvec!(fact),
                )?;
                patch.add_edge(OutletId::new(cast, 0), InletId::new(bin, i))?;
            } else {
                patch.add_edge(tap, InletId::new(bin, i))?;
            }
        }
        Ok(Some(patch))
    }

    to_typed!();
}

impl StatelessOp for InferenceBinOp {
    fn eval(&self, inputs: TVec<Arc<Tensor>>) -> TractResult<TVec<Arc<Tensor>>> {
        self.0.eval_broadcast_and_typecast(inputs)
    }
}

impl InferenceRulesOp for InferenceBinOp {
    fn rules<'r, 'p: 'r, 's: 'r>(
        &'s self,
        s: &mut Solver<'r>,
        inputs: &'p [TensorProxy],
        outputs: &'p [TensorProxy],
    ) -> InferenceResult {
        check_input_arity(&inputs, 2)?;
        check_output_arity(&outputs, 1)?;

        s.with(&inputs[0].shape, move |s, a_shape| {
            s.with(&inputs[1].shape, move |s, b_shape| {
                if let Ok(Some(c_shape)) =
                    crate::analyser::helpers::infer_shape_broadcasting(&[&a_shape, &b_shape])
                {
                    s.equals(&outputs[0].shape, c_shape)?;
                }
                Ok(())
            })
        })?;
        s.given_2(&inputs[0].datum_type, &inputs[1].datum_type, move |s, typa, typb| {
            s.equals(&outputs[0].datum_type, self.0.result_datum_type(typa, typb)?)
        })?;
        Ok(())
    }
    inference_op_as_op!();
}

impl TypedOp for InferenceBinOp {
    typed_op_as_op!();
}

#[derive(Debug, Clone)]
pub struct TypedBinOp(pub Box<dyn BinMiniOp>);

impl Op for TypedBinOp {
    fn name(&self) -> Cow<str> {
        format!("{}Typed", self.0.name()).into()
    }

    fn declutter(
        &self,
        model: &TypedModel,
        node: &TypedNode,
    ) -> TractResult<Option<TypedModelPatch>> {
        let inputs = model.node_input_facts(node.id)?;
        if let Some(b) = inputs[1].konst.clone() {
            let op = UnaryAOp(self.0.clone(), b.clone());
            return Ok(Some(TypedModelPatch::replace_single_op(&model, &node, &node.inputs[0..1], op)?));
        }
        if inputs[0].shape == inputs[1].shape {
            let op = MergeOp(self.0.clone());
            return Ok(Some(TypedModelPatch::replace_single_op(&model, &node, &node.inputs, op)?));
        }
        Ok(None)
    }

    to_typed!();
}

impl StatelessOp for TypedBinOp {
    fn eval(&self, inputs: TVec<Arc<Tensor>>) -> TractResult<TVec<Arc<Tensor>>> {
        self.0.eval_broadcast(inputs)
    }
}

impl TypedOp for TypedBinOp {
    typed_op_as_op!();
}

#[derive(Debug, Clone)]
pub struct UnaryAOp(pub Box<dyn BinMiniOp>, Arc<Tensor>);

impl Op for UnaryAOp {
    fn name(&self) -> Cow<str> {
        format!("{}UnaryA", self.0.name()).into()
    }

    fn pulsify(
        &self,
        _source: &NormalizedModel,
        node: &NormalizedNode,
        target: &mut PulsedModel,
        mapping: &HashMap<OutletId, OutletId>,
    ) -> TractResult<TVec<OutletId>> {
        let input = mapping[&node.inputs[0]];
        let mut fact = target.outlet_fact(input)?.clone();
        fact.dt = self.1.datum_type();
        let id = target.chain_after(input, &*node.name, self.clone(), tvec!(fact))?;
        Ok(tvec!(OutletId::new(id, 0)))
    }

    to_typed!();
}

impl StatelessOp for UnaryAOp {
    fn eval(&self, inputs: TVec<Arc<Tensor>>) -> TractResult<TVec<Arc<Tensor>>> {
        self.0.eval_broadcast(tvec!(inputs[0].clone(), self.1.clone()))
    }
}

impl TypedOp for UnaryAOp {
    typed_op_as_op!();
}

#[derive(Debug, Clone)]
pub struct MergeOp(pub Box<dyn BinMiniOp>);

impl Op for MergeOp {
    fn name(&self) -> Cow<str> {
        format!("{}Merge", self.0.name()).into()
    }

    fn pulsify(
        &self,
        _source: &NormalizedModel,
        node: &NormalizedNode,
        target: &mut PulsedModel,
        mapping: &HashMap<OutletId, OutletId>,
    ) -> TractResult<TVec<OutletId>> {
        use crate::pulse::delay::Delay;
        let delay = (0..2)
            .map(|ix| target.outlet_fact(mapping[&node.inputs[ix]]).unwrap().delay)
            .max()
            .unwrap();
        let mut output_fact = target.outlet_fact(mapping[&node.inputs[0]])?.clone();
        output_fact.delay = delay;
        let id = target.add_node(&*node.name, self.clone(), tvec!(output_fact))?;
        for ix in 0..2 {
            let input = mapping[&node.inputs[ix]];
            let input_delay = target.outlet_fact(input)?.delay;
            let source = if input_delay < delay {
                let add_delay = delay - input_delay;
                let fact = target.outlet_fact(input)?.clone();
                let mut fixed_fact = fact.clone();
                fixed_fact.delay += add_delay;
                let id = target.chain_after(
                    mapping[&node.inputs[ix]],
                    format!("{}/Delay", &*node.name),
                    Delay::new(fact, add_delay, 0),
                    tvec!(fixed_fact),
                )?;
                OutletId::new(id, 0)
            } else {
                input
            };
            target.add_edge(source, InletId::new(id, ix))?;
        }
        Ok(tvec!(OutletId::new(id, 0)))
    }

    to_typed!();
}

impl StatelessOp for MergeOp {
    fn eval(&self, inputs: TVec<Arc<Tensor>>) -> TractResult<TVec<Arc<Tensor>>> {
        self.0.eval_broadcast(inputs)
    }
}

impl TypedOp for MergeOp {
    typed_op_as_op!();
}

#[macro_export]
macro_rules! bin_to_super_type {
    ($func:ident, $Op:ident, $( [$($typ:ident),*] => $cab:expr ),*) => {
        #[derive(Debug, Clone)]
        struct $Op;
        impl $crate::ops::binary::BinMiniOp for $Op {
            fn name(&self) -> &'static str {
                stringify!($Op)
            }

            fn eval_out_of_place(&self, c: &mut Tensor, a: &Tensor, b: &Tensor) -> TractResult<()> {
                $(
                    $(if c.datum_type() == $typ::datum_type() {
                        let a = a.to_array_view::<$typ>()?;
                        let b = b.to_array_view::<$typ>()?;
                        let mut c = c.to_array_view_mut::<$typ>()?;
                        ndarray::Zip::from(&mut c).and_broadcast(a).and_broadcast(b).apply($cab);
                        return Ok(())
                    }
                    )*
                )*
                bail!("{} does not support {:?}", self.name(), c.datum_type());
            }

            fn operating_datum_type(&self, a: DatumType, b: DatumType) -> TractResult<DatumType> {
                a.common_super_type(b).ok_or_else(|| format!("No super type for {:?} and {:?}", a, b).into())
            }

            fn result_datum_type(&self, a: DatumType, b: DatumType) -> TractResult<DatumType> {
                self.operating_datum_type(a, b)
            }
        }

        pub fn $func() -> impl InferenceOp {
            $crate::ops::binary::InferenceBinOp(Box::new($Op))
        }
    };
}

macro_rules! bin_to_bool {
    ($func:ident, $Op:ident, $( [$($typ:ident),*] => $cab:expr ),*) => {
        #[derive(Debug, Clone)]
        struct $Op;
        impl BinMiniOp for $Op {
            fn name(&self) -> &'static str {
                stringify!($Op)
            }

            fn eval_out_of_place(&self, c: &mut Tensor, a: &Tensor, b: &Tensor) -> TractResult<()> {
                $(
                    $(if a.datum_type() == $typ::datum_type() {
                        let a = a.to_array_view::<$typ>()?;
                        let b = b.to_array_view::<$typ>()?;
                        let mut c = c.to_array_view_mut::<bool>()?;
                        ndarray::Zip::from(&mut c).and_broadcast(a).and_broadcast(b).apply($cab);
                        return Ok(())
                    }
                    )*
                )*
                bail!("{} does not support {:?}", self.name(), c.datum_type());
            }

            fn operating_datum_type(&self, a: DatumType, b: DatumType) -> TractResult<DatumType> {
                a.common_super_type(b).ok_or_else(|| format!("No super type for {:?} and {:?}", a, b).into())
            }

            fn result_datum_type(&self, _a: DatumType, _b: DatumType) -> TractResult<DatumType> {
                Ok(bool::datum_type())
            }
        }

        pub fn $func() -> impl InferenceOp {
            InferenceBinOp(Box::new($Op))
        }
    };
}

bin_to_super_type!(add, Add,
     [f32, i8, i16, i32, i64, u8, u16, f16, f64, TDim] => |c, a, b| *c = a.clone() + b);
bin_to_super_type!(sub, Sub,
     [f32, i8, i16, i32, i64, u8, u16, f16, f64, TDim] => |c, a, b| *c = a.clone() - b);
bin_to_super_type!(mul, Mul,
     [f32, i8, i16, i32, i64, u8, u16, f16, f64, TDim] => |c, a, b| *c = a.clone() * b);
bin_to_super_type!(div, Div,
     [f32, i8, i16, i32, i64, u8, u16, f16, f64, TDim] => |c, a, b| *c = a.clone() / b);
bin_to_super_type!(rem, Rem,
     [f32, i8, i16, i32, i64, u8, u16, f16, f64, TDim] => |c, a, b| *c = a.clone() % b);
bin_to_super_type!(min, Min,
     [f32, f64] => |c,a,b| *c = a.min(*b),
     [i8, i16, i32, i64, u8, u16] => |c, a, b| *c = *a.min(b));
bin_to_super_type!(max, Max,
     [f32, f64] => |c,a,b| *c = a.max(*b),
     [i8, i16, i32, i64, u8, u16] => |c, a, b| *c = *a.max(b));
bin_to_super_type!(pow, Pow,
     [f32, f64] => |c,a,b| *c = a.powf(*b));
bin_to_super_type!(and, And,
     [bool, u8, i8, i16, i32, i64] => |c, &a, &b| *c = (a as i64 != 0 && b as i64 != 0) as _);
bin_to_super_type!(or, Or,
     [bool, u8, i8, i16, i32, i64] => |c, &a, &b| *c = (a as i64 != 0 || b as i64 != 0) as _);
bin_to_super_type!(xor, Xor, [bool] => |c, &a, &b| *c = a ^ b);
bin_to_bool!(equals, Equals,
     [bool, u8, i8, i16, i32, i64, f32, f64, TDim] => |c, a, b | *c = a == b
);
bin_to_bool!(lesser, Lesser, [bool, u8, i8, i16, i32, i64, f32, f64] => |c, &a, &b | *c = a < b);
bin_to_bool!(lesser_equal, LesserEqual, [bool, u8, i8, i16, i32, i64, f32, f64] => |c, &a, &b | *c = a <= b);
bin_to_bool!(greater, Greatser, [bool, u8, i8, i16, i32, i64, f32, f64] => |c, &a, &b | *c = a > b);
bin_to_bool!(greater_equal, GreaterEqual, [bool, u8, i8, i16, i32, i64, f32, f64] => |c, &a, &b | *c = a >= b);