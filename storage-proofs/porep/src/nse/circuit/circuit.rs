use bellperson::{
    gadgets::{boolean::Boolean, num},
    Circuit, ConstraintSystem, SynthesisError,
};
use ff::Field;
use generic_array::typenum::Unsigned;
use paired::bls12_381::{Bls12, Fr};
use storage_proofs_core::{
    compound_proof::CircuitComponent,
    gadgets::{constraint, por::enforce_inclusion, uint64::UInt64},
    hasher::{HashFunction, Hasher, PoseidonFunction, PoseidonMDArity},
    merkle::MerkleTreeTrait,
    proof::ProofScheme,
    util::reverse_bit_numbering,
};

use super::{hash::*, LayerProof, NodeProof};
use crate::nse::{Config, NarrowStackedExpander};

/// NSE Circuit.
pub struct NseCircuit<'a, Tree: 'static + MerkleTreeTrait, G: 'static + Hasher> {
    pub(crate) public_params: <NarrowStackedExpander<'a, Tree, G> as ProofScheme<'a>>::PublicParams,
    pub(crate) replica_id: Option<<Tree::Hasher as Hasher>::Domain>,
    pub(crate) comm_r: Option<<Tree::Hasher as Hasher>::Domain>,
    pub(crate) comm_d: Option<G::Domain>,

    pub(crate) layer_proofs: Vec<LayerProof<Tree, G>>,
    pub(crate) comm_layers: Vec<Option<<Tree::Hasher as Hasher>::Domain>>,
}

impl<'a, Tree: 'static + MerkleTreeTrait, G: 'static + Hasher> CircuitComponent
    for NseCircuit<'a, Tree, G>
{
    type ComponentPrivateInputs = ();
}

impl<'a, Tree: 'static + MerkleTreeTrait, G: 'static + Hasher> Circuit<Bls12>
    for NseCircuit<'a, Tree, G>
{
    fn synthesize<CS: ConstraintSystem<Bls12>>(self, cs: &mut CS) -> Result<(), SynthesisError> {
        let Self {
            replica_id,
            comm_r,
            comm_d,
            layer_proofs,
            comm_layers,
            public_params,
            ..
        } = self;

        // Allocate replica_id
        let replica_id_num = num::AllocatedNum::alloc(cs.namespace(|| "replica_id"), || {
            replica_id
                .map(Into::into)
                .ok_or_else(|| SynthesisError::AssignmentMissing)
        })?;

        // make replica_id a public input
        replica_id_num.inputize(cs.namespace(|| "replica_id_input"))?;

        // get the replica_id in bits
        let replica_id_bits =
            reverse_bit_numbering(replica_id_num.to_bits_le(cs.namespace(|| "replica_id_bits"))?);

        // comm_d
        // Allocate comm_d as Fr
        let comm_d_num = num::AllocatedNum::alloc(cs.namespace(|| "comm_d"), || {
            comm_d
                .map(Into::into)
                .ok_or_else(|| SynthesisError::AssignmentMissing)
        })?;

        // make comm_d a public input
        comm_d_num.inputize(cs.namespace(|| "comm_d_input"))?;

        // Allocate comm_r as Fr
        let comm_r_num = num::AllocatedNum::alloc(cs.namespace(|| "comm_r"), || {
            comm_r
                .map(Into::into)
                .ok_or_else(|| SynthesisError::AssignmentMissing)
        })?;

        // make comm_r a public input
        comm_r_num.inputize(cs.namespace(|| "comm_r_input"))?;

        // Allocate comm_layers
        let comm_layers_nums = comm_layers
            .into_iter()
            .enumerate()
            .map(|(i, comm)| {
                num::AllocatedNum::alloc(cs.namespace(|| format!("comm_layer_{}", i)), || {
                    comm.map(Into::into)
                        .ok_or_else(|| SynthesisError::AssignmentMissing)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut comm_layers_nums_padded = comm_layers_nums.clone();
        let arity = PoseidonMDArity::to_usize();
        while comm_layers_nums_padded.len() % arity != 0 {
            comm_layers_nums_padded.push(num::AllocatedNum::alloc(
                cs.namespace(|| format!("padding_{}", comm_layers_nums_padded.len())),
                || Ok(Fr::zero()),
            )?);
        }

        // Verify hash(comm_layers) == comm_r
        {
            let hash_num = PoseidonFunction::hash_md_circuit::<_>(
                &mut cs.namespace(|| "comm_layers_hash"),
                &comm_layers_nums_padded,
            )?;
            constraint::equal(
                cs,
                || "enforce comm_r = H(comm_layers)",
                &comm_r_num,
                &hash_num,
            );
        }

        // Verify each layer proof
        for (i, layer_proof) in layer_proofs.into_iter().enumerate() {
            layer_proof.synthesize(
                &mut cs.namespace(|| format!("layer_proof_{}", i)),
                &public_params.config,
                &comm_d_num,
                &comm_layers_nums,
                &replica_id_bits,
            )?;
        }

        Ok(())
    }
}

impl<'a, Tree: 'static + MerkleTreeTrait, G: 'static + Hasher> LayerProof<Tree, G> {
    fn synthesize<CS: ConstraintSystem<Bls12>>(
        self,
        cs: &mut CS,
        config: &Config,
        comm_d: &num::AllocatedNum<Bls12>,
        comm_layers_nums: &[num::AllocatedNum<Bls12>],
        replica_id: &[Boolean],
    ) -> Result<(), SynthesisError> {
        let Self {
            first_layer_proof,
            expander_layer_proofs,
            butterfly_layer_proofs,
            last_layer_proof,
        } = self;

        {
            let challenge_num = UInt64::alloc(
                cs.namespace(|| "first_layer_challenge_num"),
                first_layer_proof.challenge,
            )?;
            challenge_num.pack_into_input(cs.namespace(|| "first_layer_challenge_input"))?;

            let layer_leaf = derive_first_layer_leaf(
                cs.namespace(|| "first_layer_leaf"),
                replica_id,
                &challenge_num,
                1,
            )?;

            first_layer_proof.synthesize(
                &mut cs.namespace(|| "first_layer"),
                comm_d,
                &comm_layers_nums[0],
                Some(&layer_leaf),
                false,
            )?;
        }

        for (i, proof) in expander_layer_proofs.into_iter().enumerate() {
            let mut cs = cs.namespace(|| format!("expander_layer_{}", i));
            let layer = i + 2;
            let challenge_num = UInt64::alloc(cs.namespace(|| "challenge_num"), proof.challenge)?;
            challenge_num.pack_into_input(cs.namespace(|| "challenge_input"))?;

            let parents_data = proof
                .parents
                .iter()
                .enumerate()
                .map(|(j, (_, leaf))| {
                    num::AllocatedNum::alloc(cs.namespace(|| format!("parents_data_{}", j)), || {
                        leaf.map(Into::into)
                            .ok_or_else(|| SynthesisError::AssignmentMissing)
                    })
                })
                .collect::<Result<Vec<num::AllocatedNum<Bls12>>, _>>()?;

            let layer_leaf = derive_expander_layer_leaf(
                cs.namespace(|| "leaf"),
                replica_id,
                &challenge_num,
                layer as u32,
                config,
                &parents_data,
            )?;

            proof.synthesize(
                &mut cs.namespace(|| "proof"),
                comm_d,
                &comm_layers_nums[layer - 1],
                Some(&layer_leaf),
                true,
            )?;
        }

        for (i, proof) in butterfly_layer_proofs.into_iter().enumerate() {
            let layer = i + config.num_expander_layers + 1;

            let challenge_num = UInt64::alloc(
                cs.namespace(|| format!("butterfly_layer_{}_challenge_num", i)),
                proof.challenge,
            )?;
            challenge_num.pack_into_input(
                cs.namespace(|| format!("butterfly_layer_{}_challenge_input", i)),
            )?;

            // let layer_leaf = derive_butterfly_layer_leaf(
            //     cs.namespace(|| format!("butterfly_layer_leaf_{}", i)),
            //     replica_id,
            //     &challenge_num,
            //     layer as u32,
            // )?;
            proof.synthesize(
                &mut cs.namespace(|| format!("butterfly_layer_{}", i)),
                comm_d,
                &comm_layers_nums[layer - i],
                None, // &layer_leaf,
                true,
            )?;
        }

        {
            let layer = config.num_layers();
            let challenge_num = UInt64::alloc(
                cs.namespace(|| "last_layer_challenge_num"),
                last_layer_proof.challenge,
            )?;
            challenge_num.pack_into_input(cs.namespace(|| "last_layer_challenge_input"))?;

            // let layer_leaf = derive_last_layer_leaf(
            //     cs.namespace(|| "last_layer_leaf"),
            //     replica_id,
            //     &challenge_num,
            //     layer as u32,
            // )?;
            last_layer_proof.synthesize(
                &mut cs.namespace(|| "last_layer"),
                comm_d,
                &comm_layers_nums[layer - 1],
                None, // &layer_leaf,
                true,
            )?;
        }

        Ok(())
    }
}

impl<'a, Tree: 'static + MerkleTreeTrait, G: 'static + Hasher> NodeProof<Tree, G> {
    fn synthesize<CS: ConstraintSystem<Bls12>>(
        self,
        cs: &mut CS,
        comm_d: &num::AllocatedNum<Bls12>,
        layer_root: &num::AllocatedNum<Bls12>,
        layer_leaf: Option<&num::AllocatedNum<Bls12>>,
        with_parents: bool,
    ) -> Result<(), SynthesisError> {
        let Self {
            data_path,
            data_leaf,
            layer_path,
            ..
        } = self;

        // -- data_proof

        // PrivateInput: data_leaf
        let data_leaf_num = num::AllocatedNum::alloc(cs.namespace(|| "data_leaf"), || {
            data_leaf.ok_or_else(|| SynthesisError::AssignmentMissing)
        })?;

        // enforce inclusion of the data leaf in the tree D
        enforce_inclusion(
            cs.namespace(|| "data_inclusion"),
            data_path,
            comm_d,
            &data_leaf_num,
        )?;

        // -- layer_proof
        if let Some(layer_leaf) = layer_leaf {
            enforce_inclusion(
                cs.namespace(|| "layer_inclusion"),
                layer_path,
                layer_root,
                layer_leaf,
            )?;
        }

        // -- parents_proofs
        if with_parents {
            // TODO:
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bellperson::util_cs::{metric_cs::MetricCS, test_cs::TestConstraintSystem};
    use ff::Field;
    use generic_array::typenum::{U0, U2, U4, U8};
    use merkletree::store::StoreConfig;
    use rand::{Rng, SeedableRng};
    use rand_xorshift::XorShiftRng;
    use storage_proofs_core::{
        cache_key::CacheKey,
        compound_proof::CompoundProof,
        fr32::fr_into_bytes,
        hasher::{Hasher, PoseidonHasher, Sha256Hasher},
        merkle::{get_base_tree_count, DiskTree, MerkleTreeTrait},
        proof::ProofScheme,
        test_helper::setup_replica,
    };

    use crate::nse::{
        circuit::NseCompound, Config, PrivateInputs, PublicInputs, SetupParams, TemporaryAux,
        TemporaryAuxCache,
    };
    use crate::PoRep;

    #[test]
    fn nse_input_circuit_poseidon_sub_8_2() {
        nse_input_circuit::<DiskTree<PoseidonHasher, U8, U2, U0>>(30, 2_410_677);
    }

    #[test]
    fn nse_input_circuit_poseidon_sub_8_4() {
        nse_input_circuit::<DiskTree<PoseidonHasher, U8, U4, U0>>(30, 2_864_935);
    }

    fn nse_input_circuit<Tree: MerkleTreeTrait + 'static>(
        expected_inputs: usize,
        expected_constraints: usize,
    ) {
        let rng = &mut XorShiftRng::from_seed(crate::TEST_SEED);
        let nodes = 8 * get_base_tree_count::<Tree>();
        let windows = Tree::SubTreeArity::to_usize();

        let replica_id: Fr = Fr::random(rng);
        let config = Config {
            k: 4,
            num_nodes_window: nodes / windows,
            degree_expander: 6,
            degree_butterfly: 4,
            num_expander_layers: 3,
            num_butterfly_layers: 2,
            sector_size: nodes * 32,
        };

        let data: Vec<u8> = (0..config.num_nodes_sector())
            .flat_map(|_| fr_into_bytes(&Fr::random(rng)))
            .collect();

        // MT for original data is always named tree-d, and it will be
        // referenced later in the process as such.
        let cache_dir = tempfile::tempdir().unwrap();
        let store_config = StoreConfig::new(
            cache_dir.path(),
            CacheKey::CommDTree.to_string(),
            StoreConfig::default_rows_to_discard(config.num_nodes_sector(), U2::to_usize()),
        );

        // Generate a replica path.
        let temp_dir = tempdir::TempDir::new("test-extract-all").unwrap();
        let temp_path = temp_dir.path();
        let replica_path = temp_path.join("replica-path");

        let mut mmapped_data = setup_replica(&data, &replica_path);

        // TODO: add porepid to NSE
        // let arbitrary_porep_id = [44; 32];
        let sp = SetupParams {
            config: config.clone(),
            num_layer_challenges: 2,
        };
        let pp = NarrowStackedExpander::<Tree, Sha256Hasher>::setup(&sp).expect("setup failed");

        let (tau, (p_aux, t_aux)) = NarrowStackedExpander::<Tree, Sha256Hasher>::replicate(
            &pp,
            &replica_id.into(),
            (mmapped_data.as_mut()).into(),
            None,
            store_config.clone(),
            replica_path.clone(),
        )
        .expect("replication failed");

        let copied = mmapped_data.to_vec();
        assert_ne!(data, copied, "replication did not change data");

        let seed = rng.gen();
        let pub_inputs =
            PublicInputs::<<Tree::Hasher as Hasher>::Domain, <Sha256Hasher as Hasher>::Domain> {
                replica_id: replica_id.into(),
                seed,
                tau,
                k: None,
            };

        // Store copy of original t_aux for later resource deletion.
        let t_aux_orig = t_aux.clone();

        // Convert TemporaryAux to TemporaryAuxCache, which instantiates all
        // elements based on the configs stored in TemporaryAux.
        let t_aux = TemporaryAuxCache::<Tree, Sha256Hasher>::new(&config, &t_aux, replica_path)
            .expect("failed to restore contents of t_aux");

        let priv_inputs = PrivateInputs::<Tree, Sha256Hasher> { p_aux, t_aux };

        let proofs = NarrowStackedExpander::<Tree, Sha256Hasher>::prove_all_partitions(
            &pp,
            &pub_inputs,
            &priv_inputs,
            1,
        )
        .expect("failed to generate partition proofs");

        let proofs_are_valid = NarrowStackedExpander::<Tree, Sha256Hasher>::verify_all_partitions(
            &pp,
            &pub_inputs,
            &proofs,
        )
        .expect("failed while trying to verify partition proofs");

        assert!(proofs_are_valid);

        // Discard cached MTs that are no longer needed.
        TemporaryAux::<Tree, Sha256Hasher>::clear_temp(t_aux_orig).expect("t_aux delete failed");

        {
            // Verify that MetricCS returns the same metrics as TestConstraintSystem.
            let mut cs = MetricCS::<Bls12>::new();

            NseCompound::circuit(&pub_inputs, (), &proofs[0], &pp, None)
                .expect("circuit failed")
                .synthesize(&mut cs.namespace(|| "nse drgporep"))
                .expect("failed to synthesize circuit");

            assert_eq!(cs.num_inputs(), expected_inputs, "wrong number of inputs");
            assert_eq!(
                cs.num_constraints(),
                expected_constraints,
                "wrong number of constraints"
            );
        }
        let mut cs = TestConstraintSystem::<Bls12>::new();

        NseCompound::circuit(&pub_inputs, (), &proofs[0], &pp, None)
            .expect("circuit failed")
            .synthesize(&mut cs.namespace(|| "nse drgporep"))
            .expect("failed to synthesize circuit");

        assert!(cs.is_satisfied(), "constraints not satisfied");
        assert_eq!(cs.num_inputs(), expected_inputs, "wrong number of inputs");
        assert_eq!(
            cs.num_constraints(),
            expected_constraints,
            "wrong number of constraints"
        );

        assert_eq!(cs.get_input(0, "ONE"), Fr::one());

        let generated_inputs = <NseCompound as CompoundProof<
            NarrowStackedExpander<Tree, Sha256Hasher>,
            _,
        >>::generate_public_inputs(&pub_inputs, &pp, None)
        .expect("failed to generate public inputs");
        let expected_inputs = cs.get_inputs();

        for ((input, label), generated_input) in
            expected_inputs.iter().skip(1).zip(generated_inputs.iter())
        {
            assert_eq!(input, generated_input, "{}", label);
        }

        assert_eq!(
            generated_inputs.len(),
            expected_inputs.len() - 1,
            "inputs are not the same length"
        );

        cache_dir.close().expect("Failed to remove cache dir");
    }
}
