use reth_primitives_traits::NodePrimitives;

/// Provider implementation that knows configured [`NodePrimitives`].
#[auto_impl::auto_impl(&, Arc, Box)]
pub trait NodePrimitivesProvider {
    /// The node primitive types.
    type Primitives: NodePrimitives;
}
