use tract_core::internal::*;

#[derive(Clone, Debug)]
pub struct KaldiProtoModel {
    pub config_lines: ConfigLines,
    pub components: HashMap<String, Component>,
}

#[derive(Clone, Debug)]
pub struct ConfigLines {
    pub input_name: String,
    pub input_dim: usize,
    pub component_nodes: HashMap<String, ComponentNode>,
    pub dim_range_nodes: HashMap<String, DimRangeNode>,
    pub output_name: String,
    pub output_input: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GeneralDescriptor {
    Append(Vec<GeneralDescriptor>),
    IfDefined(Box<GeneralDescriptor>),
    Name(String),
    Offset(Box<GeneralDescriptor>, isize),
}

impl GeneralDescriptor {
    pub fn inputs(&self) -> TVec<&str> {
        match self {
            GeneralDescriptor::Append(ref gds) => gds.iter().fold(tvec!(), |mut acc, gd| {
                gd.inputs().iter().for_each(|i| {
                    if !acc.contains(i) {
                        acc.push(i)
                    }
                });
                acc
            }),
            GeneralDescriptor::IfDefined(ref gd) => gd.inputs(),
            GeneralDescriptor::Name(ref s) => tvec!(&**s),
            GeneralDescriptor::Offset(ref gd, _) => gd.inputs(),
        }
    }

    pub fn as_conv_shape_dilation(&self) -> Option<(usize, usize)> {
        if let GeneralDescriptor::Name(_) = self {
            return Some((1, 1));
        }
        if let GeneralDescriptor::Append(ref appendees) = self {
            let mut offsets = vec![];
            for app in appendees {
                match app {
                    GeneralDescriptor::Name(_) => offsets.push(0),
                    GeneralDescriptor::Offset(_, offset) => offsets.push(*offset),
                    _ => return None,
                }
            }
            let dilation = offsets[1] - offsets[0];
            if offsets.windows(2).all(|pair| pair[1] - pair[0] == dilation) {
                return Some((offsets.len(), dilation as usize));
            }
        }
        return None;
    }

    fn wire<'a>(
        &'a self,
        inlet: InletId,
        name: &str,
        model: &mut InferenceModel,
        deferred: &mut HashMap<InletId, &'a str>,
    ) -> TractResult<()> {
        use GeneralDescriptor::*;
        match &self {
            &Name(n) => {
                if let Ok(id) = model.node_by_name(&n).map(|n| n.id) {
                    model.add_edge(OutletId::new(id, 0), inlet)?;
                } else {
                    deferred.insert(inlet, &n);
                }
                return Ok(());
            }
            &Append(appendees) => {
                let name = format!("{}-Append", name);
                let id = model.add_node_default(&*name, tract_core::ops::array::Concat::new(1))?;
                model.add_edge(OutletId::new(id, 0), inlet)?;
                for (ix, appendee) in appendees.iter().enumerate() {
                    let name = format!("{}-{}", name, ix);
                    appendee.wire(InletId::new(id, ix), &*name, model, deferred)?;
                }
                return Ok(());
            }
            &IfDefined(ref o) => {
                if let &Offset(ref n, ref o) = &**o {
                    if let Name(n) = &**n {
                        let name = format!("{}-Memory", name);
                        let id = model.add_node_default(
                            &*name,
                            crate::ops::memory::Memory::new(n.to_string(), *o),
                        )?;
                        model.add_edge(OutletId::new(id, 0), inlet)?;
                        return Ok(());
                    }
                }
            }
            _ => (),
        }
        bail!("Unhandled input descriptor: {:?}", self)
    }
}

#[derive(Clone, Debug)]
pub struct DimRangeNode {
    pub input: GeneralDescriptor,
    pub offset: usize,
    pub dim: usize,
}

#[derive(Clone, Debug)]
pub struct ComponentNode {
    pub input: GeneralDescriptor,
    pub component: String,
}

#[derive(Clone, Debug, Default)]
pub struct Component {
    pub klass: String,
    pub attributes: HashMap<String, Arc<Tensor>>,
}

pub struct ParsingContext<'a> {
    pub proto_model: &'a KaldiProtoModel,
}

#[derive(Clone, Default)]
pub struct KaldiOpRegister(
    pub HashMap<String, fn(&ParsingContext, node: &str) -> TractResult<Box<InferenceOp>>>,
);

impl KaldiOpRegister {
    pub fn insert(
        &mut self,
        s: &'static str,
        builder: fn(&ParsingContext, node: &str) -> TractResult<Box<InferenceOp>>,
    ) {
        self.0.insert(s.into(), builder);
    }
}

#[derive(Clone, Default)]
pub struct Kaldi {
    pub op_register: KaldiOpRegister,
}

impl Framework<KaldiProtoModel> for Kaldi {
    fn proto_model_for_read(&self, r: &mut std::io::Read) -> TractResult<KaldiProtoModel> {
        use crate::parser;
        let mut v = vec![];
        r.read_to_end(&mut v)?;
        parser::nnet3(&*v)
    }

    fn model_for_proto_model(&self, proto_model: &KaldiProtoModel) -> TractResult<InferenceModel> {
        let ctx = ParsingContext { proto_model };
        let mut model = InferenceModel::default();
        model.add_source(
            proto_model.config_lines.input_name.clone(),
            TensorFact::dt_shape(
                f32::datum_type(),
                shapefact!(S, (proto_model.config_lines.input_dim)),
            ),
        )?;
        let mut inputs_to_wire: HashMap<InletId, &str> = HashMap::new();
        for (name, node) in &proto_model.config_lines.component_nodes {
            let component = &proto_model.components[&node.component];
            if crate::ops::AFFINE.contains(&&*component.klass)
                && node.input.as_conv_shape_dilation().is_some()
            {
                let op = crate::ops::affine::affine_component(&ctx, name)?;
                let id = model.add_node_default(name.to_string(), op)?;
                inputs_to_wire.insert(InletId::new(id, 0), node.input.inputs()[0]);
            } else {
                let op = match self.op_register.0.get(&*component.klass) {
                    Some(builder) => (builder)(&ctx, name)?,
                    None => {
                        (Box::new(tract_core::ops::unimpl::UnimplementedOp::new(
                            component.klass.to_string(),
                            format!("{:?}", proto_model.config_lines.component_nodes.get(name)),
                        )))
                    }
                };
                let id = model.add_node_default(name.to_string(), op)?;
                node.input.wire(InletId::new(id, 0), name, &mut model, &mut inputs_to_wire)?
            }
        }
        for (name, node) in &proto_model.config_lines.dim_range_nodes {
            let op = tract_core::ops::array::Slice::new(
                Some(vec![1]),
                vec![node.offset as isize],
                vec![(node.offset + node.dim) as isize],
            );
            let id = model.add_node_default(name.to_string(), op)?;
            node.input.wire(InletId::new(id, 0), name, &mut model, &mut inputs_to_wire)?
        }
        for (inlet, name) in inputs_to_wire {
            let src = OutletId::new(model.node_by_name(name)?.id, 0);
            model.add_edge(src, inlet)?;
        }
        let output = model.add_node_default(
            proto_model.config_lines.output_name.to_string(),
            tract_core::ops::identity::Identity::default(),
        )?;
        let src = OutletId::new(model.node_by_name(&*proto_model.config_lines.output_input)?.id, 0);
        let dst = InletId::new(output, 0);
        model.add_edge(src, dst)?;
        model.set_output_outlets(&[OutletId::new(output, 0)])?;
        Ok(model)
    }
}