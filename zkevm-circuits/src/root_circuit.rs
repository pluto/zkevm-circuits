//! The Root circuit implementation.
use eth_types::Field;
use halo2_proofs::{
    arithmetic::Field as Halo2Field,
    circuit::{Layouter, SimpleFloorPlanner, Value},
    halo2curves::{serde::SerdeObject, CurveAffine},
    plonk::{Circuit, ConstraintSystem, Error},
    poly::{commitment::ParamsProver, kzg::commitment::ParamsKZG},
};
use itertools::Itertools;
use maingate::MainGateInstructions;

use snark_verifier::{
    pcs::{
        kzg::{KzgAccumulator, KzgAsProvingKey, KzgAsVerifyingKey, KzgDecidingKey},
        AccumulationDecider, AccumulationScheme, AccumulationSchemeProver,
        PolynomialCommitmentScheme,
    },
    util::arithmetic::MultiMillerLoop,
    verifier::plonk::PlonkProtocol, halo2_base::utils::BigPrimeField,
};
use std::{iter, marker::PhantomData, rc::Rc};

mod aggregation;

#[cfg(any(test, feature = "test-circuits"))]
mod dev;
#[cfg(test)]
mod test;
#[cfg(feature = "test-circuits")]
pub use self::RootCircuit as TestRootCircuit;

// #[cfg(any(feature = "test-circuits", test))]
// pub use dev::TestAggregationCircuit;

pub use aggregation::{
    aggregate, AggregationConfig, Gwc, Halo2Loader, KzgDk, KzgSvk, PlonkSuccinctVerifier,
    PlonkVerifier, PoseidonTranscript, Shplonk, Snark, SnarkWitness, BITS, LIMBS, SECURE_MDS
};
pub use snark_verifier::{
    loader::native::NativeLoader,
    system::halo2::{compile, transcript::evm::EvmTranscript, Config},
};

/// RootCircuit for aggregating SuperCircuit into a much smaller proof.
#[derive(Clone)]
pub struct RootCircuit<'a, M: MultiMillerLoop, As> where M::G1Affine: CurveAffine<ScalarExt = M::Fr, CurveExt = M::G1> {
    svk: KzgSvk<M>,
    snark: SnarkWitness<'a, M::G1Affine>,
    instance: Vec<M::Fr>,
    _marker: PhantomData<As>,
}

impl<'a, M, As> RootCircuit<'a, M, As>
where
    M: MultiMillerLoop,
    M::G1Affine: CurveAffine<ScalarExt = M::Fr, CurveExt = M::G1> + SerdeObject,
    M::G2Affine: CurveAffine<ScalarExt = M::Fr, CurveExt = M::G2> + SerdeObject,
    M::Fr: Field,
    As: PolynomialCommitmentScheme<
            M::G1Affine,
            NativeLoader,
            VerifyingKey = KzgSvk<M>,
            Output = KzgAccumulator<M::G1Affine, NativeLoader>,
        > + AccumulationSchemeProver<
            M::G1Affine,
            Accumulator = KzgAccumulator<M::G1Affine, NativeLoader>,
            ProvingKey = KzgAsProvingKey<M::G1Affine>,
        > + AccumulationDecider<M::G1Affine, NativeLoader, DecidingKey = KzgDecidingKey<M>>,
{
    /// Create a `RootCircuit` with accumulator computed given a `SuperCircuit`
    /// proof and its instance. Returns `None` if given proof is invalid.
    pub fn new(
        params: &ParamsKZG<M>,
        super_circuit_protocol: &'a PlonkProtocol<M::G1Affine>,
        super_circuit_instances: Value<&'a Vec<Vec<M::Fr>>>,
        super_circuit_proof: Value<&'a [u8]>,
    ) -> Result<Self, snark_verifier::Error> {
        let num_instances = super_circuit_protocol.num_instance.iter().sum::<usize>() + 4 * LIMBS;
        let instance = {
            let mut instance = Ok(vec![M::Fr::ZERO; num_instances]);
            super_circuit_instances
                .as_ref()
                .zip(super_circuit_proof.as_ref())
                .map(|(super_circuit_instances, super_circuit_proof)| {
                    println!(" === DEBUG (ROOT CIRCUIT): Aggregate instance i={:?} proof_len={}", super_circuit_instances, super_circuit_proof.len());
                    let snark = Snark::new(
                        super_circuit_protocol,
                        super_circuit_instances,
                        super_circuit_proof,
                    );
                    instance = aggregate::<M, As>(params, [snark]).map(|accumulator_limbs| {
                        iter::empty()
                            // Propagate `SuperCircuit`'s instance
                            .chain(super_circuit_instances.iter().flatten().cloned())
                            // Output aggregated accumulator limbs
                            .chain(accumulator_limbs)
                            .collect_vec()
                    });
                });
            instance?
        };
        println!(" === DEBUG (ROOT CIRCUIT): Completed aggregate");
        debug_assert_eq!(instance.len(), num_instances);

        Ok(Self {
            svk: KzgSvk::<M>::new(params.get_g()[0]),
            snark: SnarkWitness::new(
                super_circuit_protocol,
                super_circuit_instances,
                super_circuit_proof,
            ),
            instance,
            _marker: PhantomData,
        })
    }

    /// Returns accumulator indices in instance columns, which will be in
    /// the last `4 * LIMBS` rows of instance column in `MainGate`.
    pub fn accumulator_indices(&self) -> Vec<(usize, usize)> {
        let offset = self.snark.protocol().num_instance.iter().sum::<usize>();
        (offset..).map(|idx| (0, idx)).take(4 * LIMBS).collect()
    }

    /// Returns number of instance
    pub fn num_instance(&self) -> Vec<usize> {
        vec![self.snark.protocol().num_instance.iter().sum::<usize>() + 4 * LIMBS]
    }

    /// Returns instance
    pub fn instance(&self) -> Vec<Vec<M::Fr>> {
        vec![self.instance.clone()]
    }
}

use halo2_proofs::halo2curves::{pairing::Engine, CurveAffineExt};
impl<'a, M, As> Circuit<M::Fr> for RootCircuit<'a, M, As>
where
    M: MultiMillerLoop,
    M::Fr: Field + BigPrimeField,
    <<M as Engine>::G1Affine as CurveAffine>::Base: BigPrimeField,
    M::G1Affine: CurveAffineExt<ScalarExt = M::Fr, CurveExt = M::G1>,
    for<> As: PolynomialCommitmentScheme<
            M::G1Affine,
            Rc<Halo2Loader<'a, M::G1Affine>>,
            VerifyingKey = KzgSvk<M>,
            Output = KzgAccumulator<M::G1Affine, Rc<Halo2Loader<'a, M::G1Affine>>>,
        > + AccumulationScheme<
            M::G1Affine,
            Rc<Halo2Loader<'a, M::G1Affine>>,
            Accumulator = KzgAccumulator<M::G1Affine, Rc<Halo2Loader<'a, M::G1Affine>>>,
            VerifyingKey = KzgAsVerifyingKey,
        >,
{
    type Config = AggregationConfig;
    type FloorPlanner = SimpleFloorPlanner;
    type Params = ();

    fn without_witnesses(&self) -> Self {
        Self {
            svk: self.svk,
            snark: self.snark.without_witnesses(),
            instance: vec![M::Fr::ZERO; self.instance.len()],
            _marker: PhantomData,
        }
    }

    fn configure(meta: &mut ConstraintSystem<M::Fr>) -> Self::Config {
        // TODO: Fixup Configure
        //
        // AggregationConfig::configure::<M::G1Affine>(meta)
        AggregationConfig {
        }
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<M::Fr>,
    ) -> Result<(), Error> {
        //
        // TODO: Fixup synthesize 
        //

        // config.load_table(&mut layouter)?;
        // let (instance, accumulator_limbs) =
        //     config.aggregate::<M, As>(&mut layouter, &self.svk, [self.snark])?;

        // // Constrain equality to instance values
        // let main_gate = config.main_gate();
        // for (row, limb) in instance
        //     .into_iter()
        //     .flatten()
        //     .flatten()
        //     .chain(accumulator_limbs)
        //     .enumerate()
        // {
        //     main_gate.expose_public(layouter.namespace(|| ""), limb, row)?;
        // }

        Ok(())
    }
}
