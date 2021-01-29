//! Here we construct a polynomial commitment that enables users to commit to a
//! single polynomial `p`, and then later provide an evaluation proof that
//! convinces verifiers that a claimed value `v` is the true evaluation of `p`
//! at a chosen point `x`. Our construction follows the template of the construction
//! proposed by Kate, Zaverucha, and Goldberg ([KZG11](http://cacr.uwaterloo.ca/techreports/2010/cacr2010-10.pdf)).
//! This construction achieves extractability in the algebraic group model (AGM).

use ark_ec::msm::{FixedBaseMSM, VariableBaseMSM};
use ark_ec::{AffineCurve, PairingEngine, ProjectiveCurve};
use ark_ff::{One, PrimeField, UniformRand, Zero};
use ark_poly::UVPolynomial;
use ark_std::{format, marker::PhantomData, ops::Div, vec};

use bench_utils::{end_timer, start_timer};

// #[cfg(feature = "parallel")]
// use rayon::prelude::*;

use ark_poly_commit::Error;
use ark_poly_commit::kzg10::Powers;
use rand::RngCore;

/// `UniversalParams` are the universal parameters for the KZG10 scheme.
#[derive(Clone, Debug)]
pub struct UniversalParams<E: PairingEngine> {
    /// Group elements of the form `{ \beta^i G }`, where `i` ranges from 0 to `degree`.
    pub powers_of_g: Vec<E::G1Affine>,
    /// The generator of G2.
    pub h: E::G2Affine,
    /// \beta times the above generator of G2.
    pub beta_h: E::G2Affine,
}

/// `VerifierKey` is used to check evaluation proofs for a given commitment.
#[derive(Clone, Debug)]
pub struct PreparedVerifierKey<E: PairingEngine> {
    /// The generator of G1.
    pub g: E::G1Affine,
    /// The generator of G2, prepared for use in pairings.
    pub prepared_h: E::G2Prepared,
    /// \beta times the above generator of G2, prepared for use in pairings.
    pub prepared_beta_h: E::G2Prepared,
}


/// `KZG10` is an implementation of the polynomial commitment scheme of
/// [Kate, Zaverucha and Goldbgerg][kzg10]
///
/// [kzg10]: http://cacr.uwaterloo.ca/techreports/2010/cacr2010-10.pdf
pub struct KZG10<E: PairingEngine, P: UVPolynomial<E::Fr>> {
    _engine: PhantomData<E>,
    _poly: PhantomData<P>,
}

impl<E, P> KZG10<E, P>
    where
        E: PairingEngine,
        P: UVPolynomial<E::Fr, Point = E::Fr>,
        for<'a, 'b> &'a P: Div<&'b P, Output = P>,
{
    /// Constructs public parameters when given as input the maximum degree `degree`
    /// for the polynomial commitment scheme.
    pub fn setup<R: RngCore>(
        max_degree: usize,
        rng: &mut R,
    ) -> Result<UniversalParams<E>, Error> {
        if max_degree < 1 {
            return Err(Error::DegreeIsZero);
        }
        let setup_time = start_timer!(|| format!("KZG10::Setup with degree {}", max_degree));
        let beta = E::Fr::rand(rng);
        let g = E::G1Projective::rand(rng);
        let h = E::G2Projective::rand(rng);

        let mut powers_of_beta = vec![E::Fr::one()];

        let mut cur = beta;
        for _ in 0..max_degree {
            powers_of_beta.push(cur);
            cur *= &beta;
        }

        let window_size = FixedBaseMSM::get_mul_window_size(max_degree + 1);

        let scalar_bits = E::Fr::size_in_bits();
        let g_time = start_timer!(|| "Generating powers of G");
        let g_table = FixedBaseMSM::get_window_table(scalar_bits, window_size, g);
        let powers_of_g = FixedBaseMSM::multi_scalar_mul::<E::G1Projective>(
            scalar_bits,
            window_size,
            &g_table,
            &powers_of_beta,
        );
        end_timer!(g_time);


        let powers_of_g = E::G1Projective::batch_normalization_into_affine(&powers_of_g);

        let h = h.into_affine();
        let beta_h = h.mul(beta).into_affine();

        let pp = UniversalParams {
            powers_of_g,
            h,
            beta_h,
        };
        end_timer!(setup_time);
        Ok(pp)
    }

    /// Outputs a commitment to `polynomial`.
    pub fn commit(
        powers: &Powers<E>,
        polynomial: &P
    ) -> Result<E::G1Affine, Error> {
        Self::check_degree_is_too_large(polynomial.degree(), powers.size())?;

        let commit_time = start_timer!(|| format!(
            "Committing to polynomial of degree {} with hiding_bound: {:?}",
            polynomial.degree(),
            hiding_bound,
        ));

        let (num_leading_zeros, plain_coeffs) =
            skip_leading_zeros_and_convert_to_bigints(polynomial);

        let msm_time = start_timer!(|| "MSM to compute commitment to plaintext poly");
        let commitment = VariableBaseMSM::multi_scalar_mul(
            &powers.powers_of_g[num_leading_zeros..],
            &plain_coeffs,
        );
        end_timer!(msm_time);
        end_timer!(commit_time);
        Ok(commitment.into())
    }

    /// Compute witness polynomial.
    ///
    /// The witness polynomial w(x) the quotient of the division (p(x) - p(z)) / (x - z)
    /// Observe that this quotient does not change with z because
    /// p(z) is the remainder term. We can therefore omit p(z) when computing the quotient.
    pub fn compute_witness_polynomial(
        p: &P,
        point: P::Point
    ) -> Result<P, Error> {
        let divisor = P::from_coefficients_vec(vec![-point, E::Fr::one()]);

        let witness_time = start_timer!(|| "Computing witness polynomial");
        let witness_polynomial = p / &divisor;
        end_timer!(witness_time);

        Ok(witness_polynomial)
    }

    pub(crate) fn open_with_witness_polynomial<'a>(
        powers: &Powers<E>,
        witness_polynomial: &P,
    ) -> Result<E::G1Affine, Error> {
        Self::check_degree_is_too_large(witness_polynomial.degree(), powers.size())?;
        let (num_leading_zeros, witness_coeffs) =
            skip_leading_zeros_and_convert_to_bigints(witness_polynomial);

        let witness_comm_time = start_timer!(|| "Computing commitment to witness polynomial");
        let w = VariableBaseMSM::multi_scalar_mul(
            &powers.powers_of_g[num_leading_zeros..],
            &witness_coeffs,
        );
        end_timer!(witness_comm_time);

        Ok(w.into_affine())
    }

    /// On input a polynomial `p` and a point `point`, outputs a proof for the same.
    pub fn open<'a>(
        powers: &Powers<E>,
        p: &P,
        point: P::Point
    ) -> Result<E::G1Affine, Error> {
        Self::check_degree_is_too_large(p.degree(), powers.size())?;
        let open_time = start_timer!(|| format!("Opening polynomial of degree {}", p.degree()));

        let witness_time = start_timer!(|| "Computing witness polynomials");
        let witness_poly = Self::compute_witness_polynomial(p, point)?;
        end_timer!(witness_time);

        let proof = Self::open_with_witness_polynomial(
            powers,
            &witness_poly
        );

        end_timer!(open_time);
        proof
    }

    // /// Verifies that `value` is the evaluation at `point` of the polynomial
    // /// committed inside `comm`.
    // pub fn check(
    //     vk: &VerifierKey<E>,
    //     comm: &Commitment<E>,
    //     point: E::Fr,
    //     value: E::Fr,
    //     proof: &Proof<E>,
    // ) -> Result<bool, Error> {
    //     let check_time = start_timer!(|| "Checking evaluation");
    //     let mut inner = comm.0.into_projective() - &vk.g.mul(value);
    //     if let Some(random_v) = proof.random_v {
    //         inner -= &vk.gamma_g.mul(random_v);
    //     }
    //     let lhs = E::pairing(inner, vk.h);
    //
    //     let inner = vk.beta_h.into_projective() - &vk.h.mul(point);
    //     let rhs = E::pairing(proof.w, inner);
    //
    //     end_timer!(check_time, || format!("Result: {}", lhs == rhs));
    //     Ok(lhs == rhs)
    // }

    pub fn aggregate_openings<R: RngCore>(
        vk: &PreparedVerifierKey<E>,
        commitments: &[E::G1Affine],
        points: &[E::Fr],
        values: &[E::Fr],
        proofs: &[E::G1Affine],
        rng: &mut R,
    ) -> (E::G1Projective, E::G1Projective) {
        let mut total_c = <E::G1Projective>::zero();
        let mut total_w = <E::G1Projective>::zero();

        let combination_time = start_timer!(|| "Combining commitments and proofs");
        let mut randomizer = E::Fr::one();
        // Instead of multiplying g in each turn, we simply accumulate
        // it's coefficients and perform a final multiplication at the end.
        let mut g_multiplier = E::Fr::zero();
        for (((c, z), v), w) in commitments.iter()
            .zip(points)
            .zip(values)
            .zip(proofs) {
            let mut temp = w.mul(*z); // $x_i [q_i(x)]_1$
            temp.add_assign_mixed(&c); // $[p_i(x)]_1 + x_i [q_i(x)]_1$
            let c = temp;
            g_multiplier += &(randomizer * v); // $r_i y_i$
            total_c += &c.mul(randomizer.into()); // $r_i [p_i(x)]_1 + r_i x_i [q_i(x)]_1$
            total_w += &w.mul(randomizer); //  $r_i [q_i(x)]_1$
            // We don't need to sample randomizers from the full field,
            // only from 128-bit strings.
            randomizer = u128::rand(rng).into();
        }
        total_c -= &vk.g.mul(g_multiplier); // $(\sum_i r_i y_i) [1]_1$
        end_timer!(combination_time);

        (total_c, total_w)
    }

    pub fn batch_check_aggregated(
        vk: &PreparedVerifierKey<E>,
        total_c: E::G1Projective,
        total_w: E::G1Projective,
    ) -> Result<bool, Error> {
        let to_affine_time = start_timer!(|| "Converting results to affine for pairing");
        let affine_points = E::G1Projective::batch_normalization_into_affine(&[total_c, -total_w]);
        let (total_c, total_w) = (affine_points[0], affine_points[1]);
        end_timer!(to_affine_time);

        let pairing_time = start_timer!(|| "Performing product of pairings");
        let result = E::product_of_pairings(&[
            (total_c.into(), vk.prepared_h.clone()),
            (total_w.into(), vk.prepared_beta_h.clone()),
        ])
            .is_one();
        end_timer!(pairing_time);
        Ok(result)
    }

    /// Check that each `proof_i` in `proofs` is a valid proof of evaluation for
    /// `commitment_i` at `point_i`.
    pub fn batch_check<R: RngCore>(
        vk: &PreparedVerifierKey<E>,
        commitments: &[E::G1Affine],
        points: &[E::Fr],
        values: &[E::Fr],
        proofs: &[E::G1Affine],
        rng: &mut R,
    ) -> Result<bool, Error> {
        let check_time =
            start_timer!(|| format!("Checking {} evaluation proofs", commitments.len()));
        let (total_c, total_w) = Self::aggregate_openings(vk, commitments, points, values, proofs, rng);
        let result = Self::batch_check_aggregated(vk, total_c, total_w)?;
        end_timer!(check_time, || format!("Result: {}", result));
        Ok(result)
    }

    pub(crate) fn check_degree_is_too_large(
        num_coefficients: usize,
        num_powers: usize,
    ) -> Result<(), Error> {
        if num_coefficients > num_powers {
            Err(Error::TooManyCoefficients {
                num_coefficients,
                num_powers,
            })
        } else {
            Ok(())
        }
    }
}

fn skip_leading_zeros_and_convert_to_bigints<F: PrimeField, P: UVPolynomial<F>>(
    p: &P,
) -> (usize, Vec<F::BigInt>) {
    let mut num_leading_zeros = 0;
    while num_leading_zeros < p.coeffs().len() && p.coeffs()[num_leading_zeros].is_zero() {
        num_leading_zeros += 1;
    }
    let coeffs = convert_to_bigints(&p.coeffs()[num_leading_zeros..]);
    (num_leading_zeros, coeffs)
}

fn convert_to_bigints<F: PrimeField>(p: &[F]) -> Vec<F::BigInt> {
    let to_bigint_time = start_timer!(|| "Converting polynomial coeffs to bigints");
    let coeffs = ark_std::cfg_iter!(p)
        .map(|s| s.into_repr())
        .collect::<Vec<_>>();
    end_timer!(to_bigint_time);
    coeffs
}

// #[cfg(test)]
// mod tests {
//     #![allow(non_camel_case_types)]
//     use crate::kzg10::*;
//     use crate::*;
//
//     use ark_bls12_377::Bls12_377;
//     use ark_bls12_381::Bls12_381;
//     use ark_bls12_381::Fr;
//     use ark_ec::PairingEngine;
//     use ark_poly::univariate::DensePolynomial as DensePoly;
//     use ark_std::test_rng;
//
//     type UniPoly_381 = DensePoly<<Bls12_381 as PairingEngine>::Fr>;
//     type UniPoly_377 = DensePoly<<Bls12_377 as PairingEngine>::Fr>;
//     type KZG_Bls12_381 = KZG10<Bls12_381, UniPoly_381>;
//
//     impl<E: PairingEngine, P: UVPolynomial<E::Fr>> KZG10<E, P> {
//         /// Specializes the public parameters for a given maximum degree `d` for polynomials
//         /// `d` should be less that `pp.max_degree()`.
//         pub(crate) fn trim(
//             pp: &UniversalParams<E>,
//             mut supported_degree: usize,
//         ) -> Result<(Powers<E>, VerifierKey<E>), Error> {
//             if supported_degree == 1 {
//                 supported_degree += 1;
//             }
//             let powers_of_g = pp.powers_of_g[..=supported_degree].to_vec();
//             let powers_of_gamma_g = (0..=supported_degree)
//                 .map(|i| pp.powers_of_gamma_g[&i])
//                 .collect();
//
//             let powers = Powers {
//                 powers_of_g: ark_std::borrow::Cow::Owned(powers_of_g),
//                 powers_of_gamma_g: ark_std::borrow::Cow::Owned(powers_of_gamma_g),
//             };
//             let vk = VerifierKey {
//                 g: pp.powers_of_g[0],
//                 gamma_g: pp.powers_of_gamma_g[&0],
//                 h: pp.h,
//                 beta_h: pp.beta_h,
//                 prepared_h: pp.prepared_h.clone(),
//                 prepared_beta_h: pp.prepared_beta_h.clone(),
//             };
//             Ok((powers, vk))
//         }
//     }
//
//     #[test]
//     fn add_commitments_test() {
//         let rng = &mut test_rng();
//         let p = DensePoly::from_coefficients_slice(&[
//             Fr::rand(rng),
//             Fr::rand(rng),
//             Fr::rand(rng),
//             Fr::rand(rng),
//             Fr::rand(rng),
//         ]);
//         let f = Fr::rand(rng);
//         let mut f_p = DensePoly::zero();
//         f_p += (f, &p);
//
//         let degree = 4;
//         let pp = KZG_Bls12_381::setup(degree, false, rng).unwrap();
//         let (powers, _) = KZG_Bls12_381::trim(&pp, degree).unwrap();
//
//         let hiding_bound = None;
//         let (comm, _) = KZG10::commit(&powers, &p, hiding_bound, Some(rng)).unwrap();
//         let (f_comm, _) = KZG10::commit(&powers, &f_p, hiding_bound, Some(rng)).unwrap();
//         let mut f_comm_2 = Commitment::empty();
//         f_comm_2 += (f, &comm);
//
//         assert_eq!(f_comm, f_comm_2);
//     }
//
//     fn end_to_end_test_template<E, P>() -> Result<(), Error>
//         where
//             E: PairingEngine,
//             P: UVPolynomial<E::Fr, Point = E::Fr>,
//             for<'a, 'b> &'a P: Div<&'b P, Output = P>,
//     {
//         let rng = &mut test_rng();
//         for _ in 0..100 {
//             let mut degree = 0;
//             while degree <= 1 {
//                 degree = usize::rand(rng) % 20;
//             }
//             let pp = KZG10::<E, P>::setup(degree, false, rng)?;
//             let (ck, vk) = KZG10::<E, P>::trim(&pp, degree)?;
//             let p = P::rand(degree, rng);
//             let hiding_bound = Some(1);
//             let (comm, rand) = KZG10::<E, P>::commit(&ck, &p, hiding_bound, Some(rng))?;
//             let point = E::Fr::rand(rng);
//             let value = p.evaluate(&point);
//             let proof = KZG10::<E, P>::open(&ck, &p, point, &rand)?;
//             assert!(
//                 KZG10::<E, P>::check(&vk, &comm, point, value, &proof)?,
//                 "proof was incorrect for max_degree = {}, polynomial_degree = {}, hiding_bound = {:?}",
//                 degree,
//                 p.degree(),
//                 hiding_bound,
//             );
//         }
//         Ok(())
//     }
//
//     fn linear_polynomial_test_template<E, P>() -> Result<(), Error>
//         where
//             E: PairingEngine,
//             P: UVPolynomial<E::Fr, Point = E::Fr>,
//             for<'a, 'b> &'a P: Div<&'b P, Output = P>,
//     {
//         let rng = &mut test_rng();
//         for _ in 0..100 {
//             let degree = 50;
//             let pp = KZG10::<E, P>::setup(degree, false, rng)?;
//             let (ck, vk) = KZG10::<E, P>::trim(&pp, 2)?;
//             let p = P::rand(1, rng);
//             let hiding_bound = Some(1);
//             let (comm, rand) = KZG10::<E, P>::commit(&ck, &p, hiding_bound, Some(rng))?;
//             let point = E::Fr::rand(rng);
//             let value = p.evaluate(&point);
//             let proof = KZG10::<E, P>::open(&ck, &p, point, &rand)?;
//             assert!(
//                 KZG10::<E, P>::check(&vk, &comm, point, value, &proof)?,
//                 "proof was incorrect for max_degree = {}, polynomial_degree = {}, hiding_bound = {:?}",
//                 degree,
//                 p.degree(),
//                 hiding_bound,
//             );
//         }
//         Ok(())
//     }
//
//     fn batch_check_test_template<E, P>() -> Result<(), Error>
//         where
//             E: PairingEngine,
//             P: UVPolynomial<E::Fr, Point = E::Fr>,
//             for<'a, 'b> &'a P: Div<&'b P, Output = P>,
//     {
//         let rng = &mut test_rng();
//         for _ in 0..10 {
//             let mut degree = 0;
//             while degree <= 1 {
//                 degree = usize::rand(rng) % 20;
//             }
//             let pp = KZG10::<E, P>::setup(degree, false, rng)?;
//             let (ck, vk) = KZG10::<E, P>::trim(&pp, degree)?;
//             let mut comms = Vec::new();
//             let mut values = Vec::new();
//             let mut points = Vec::new();
//             let mut proofs = Vec::new();
//             for _ in 0..10 {
//                 let p = P::rand(degree, rng);
//                 let hiding_bound = Some(1);
//                 let (comm, rand) = KZG10::<E, P>::commit(&ck, &p, hiding_bound, Some(rng))?;
//                 let point = E::Fr::rand(rng);
//                 let value = p.evaluate(&point);
//                 let proof = KZG10::<E, P>::open(&ck, &p, point, &rand)?;
//
//                 assert!(KZG10::<E, P>::check(&vk, &comm, point, value, &proof)?);
//                 comms.push(comm);
//                 values.push(value);
//                 points.push(point);
//                 proofs.push(proof);
//             }
//             assert!(KZG10::<E, P>::batch_check(
//                 &vk, &comms, &points, &values, &proofs, rng
//             )?);
//         }
//         Ok(())
//     }
//
//     #[test]
//     fn end_to_end_test() {
//         end_to_end_test_template::<Bls12_377, UniPoly_377>().expect("test failed for bls12-377");
//         end_to_end_test_template::<Bls12_381, UniPoly_381>().expect("test failed for bls12-381");
//     }
//
//     #[test]
//     fn linear_polynomial_test() {
//         linear_polynomial_test_template::<Bls12_377, UniPoly_377>()
//             .expect("test failed for bls12-377");
//         linear_polynomial_test_template::<Bls12_381, UniPoly_381>()
//             .expect("test failed for bls12-381");
//     }
//     #[test]
//     fn batch_check_test() {
//         batch_check_test_template::<Bls12_377, UniPoly_377>().expect("test failed for bls12-377");
//         batch_check_test_template::<Bls12_381, UniPoly_381>().expect("test failed for bls12-381");
//     }
// }
