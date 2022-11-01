use crate::merkle::MerkleTree;
use crate::Column;
use ark_ff::Field;
use ark_ff::Zero;
use ark_poly::domain::Radix2EvaluationDomain;
use ark_poly::univariate::DensePolynomial;
use ark_poly::DenseUVPolynomial;
use ark_poly::EvaluationDomain;
use ark_poly::Polynomial;
use ark_serialize::CanonicalSerialize;
use digest::Digest;
use fast_poly::allocator::PageAlignedAllocator;
use fast_poly::plan::GpuFft;
use fast_poly::plan::GpuIfft;
use fast_poly::plan::PLANNER;
use fast_poly::stage::AddAssignStage;
use fast_poly::utils::buffer_mut_no_copy;
use fast_poly::utils::buffer_no_copy;
use fast_poly::GpuField;
use fast_poly::GpuVec;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use std::cmp::Ordering;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ops::Index;
use std::ops::IndexMut;
use std::time::Instant;

/// Matrix is an array of columns.
pub struct Matrix<F>(pub Vec<GpuVec<F>>);

impl<F: GpuField> Matrix<F> {
    pub fn new(cols: Vec<GpuVec<F>>) -> Self {
        Matrix(cols)
    }

    pub fn from_rows(rows: Vec<Vec<F>>) -> Self {
        let num_rows = rows.len();
        let num_cols = rows.first().map(|first| first.len()).unwrap_or(0);
        let mut cols = (0..num_cols)
            .map(|_| Vec::with_capacity_in(num_rows, PageAlignedAllocator))
            .collect::<Vec<GpuVec<F>>>();
        // TODO: parallelise
        for row in rows {
            debug_assert_eq!(row.len(), num_cols);
            for (col, value) in cols.iter_mut().zip(row) {
                col.push(value)
            }
        }
        Matrix::new(cols)
    }

    // TODO: perhaps bring naming of rows and cols in line with
    // how the trace is names i.e. len and width.
    pub fn num_rows(&self) -> usize {
        if self.0.is_empty() {
            return 0;
        }
        // Check all columns have the same length
        let expected_len = self.0[0].len();
        assert!(self.0.iter().all(|col| col.len() == expected_len));
        expected_len
    }

    pub fn append(&mut self, other: Matrix<F>) {
        for col in other.0 {
            self.0.push(col)
        }
    }

    pub fn join(mut matrices: Vec<Matrix<F>>) -> Matrix<F> {
        let mut accumulator = Vec::new();
        for matrix in &mut matrices {
            accumulator.append(matrix)
        }
        Matrix::new(accumulator)
    }

    pub fn num_cols(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.num_rows() == 0
    }

    #[cfg(feature = "gpu")]
    fn into_polynomials_gpu(mut self, domain: Radix2EvaluationDomain<F>) -> Self {
        let mut ifft = GpuIfft::from(domain);

        for column in &mut self.0 {
            ifft.encode(column);
        }

        ifft.execute();

        self
    }

    #[cfg(not(feature = "gpu"))]
    fn into_polynomials_cpu(mut self, domain: Radix2EvaluationDomain<F>) -> Self {
        for column in &mut self.0 {
            domain.ifft_in_place(column);
        }
        self
    }

    /// Interpolates the columns of the polynomials over the domain
    pub fn into_polynomials(self, domain: Radix2EvaluationDomain<F>) -> Self {
        #[cfg(not(feature = "gpu"))]
        return self.into_polynomials_cpu(domain);
        #[cfg(feature = "gpu")]
        return self.into_polynomials_gpu(domain);
    }

    /// Interpolates the columns of the matrix over the domain
    pub fn interpolate(&self, domain: Radix2EvaluationDomain<F>) -> Self {
        self.clone().into_polynomials(domain)
    }

    #[cfg(not(feature = "gpu"))]
    fn into_evaluations_cpu(mut self, domain: Radix2EvaluationDomain<F>) -> Self {
        for column in &mut self.0 {
            domain.fft_in_place(column);
        }
        self
    }

    #[cfg(feature = "gpu")]
    fn into_evaluations_gpu(mut self, domain: Radix2EvaluationDomain<F>) -> Self {
        let mut fft = GpuFft::from(domain);

        for column in &mut self.0 {
            fft.encode(column);
        }

        fft.execute();

        self
    }

    /// Evaluates the columns of the matrix
    pub fn into_evaluations(self, domain: Radix2EvaluationDomain<F>) -> Self {
        #[cfg(not(feature = "gpu"))]
        return self.into_evaluations_cpu(domain);
        #[cfg(feature = "gpu")]
        return self.into_evaluations_gpu(domain);
    }

    /// Evaluates the columns of the matrix
    pub fn evaluate(&self, domain: Radix2EvaluationDomain<F>) -> Self {
        self.clone().into_evaluations(domain)
    }

    #[cfg(not(feature = "gpu"))]
    pub fn sum_columns_cpu(&self) -> Matrix<F> {
        let n = self.num_rows();
        let mut accumulator = Vec::with_capacity_in(n, PageAlignedAllocator);
        accumulator.resize(n, F::zero());

        if !self.num_cols().is_zero() {
            #[cfg(not(feature = "parallel"))]
            let chunk_size = accumulator.len();
            #[cfg(feature = "parallel")]
            let chunk_size = std::cmp::max(
                accumulator.len() / rayon::current_num_threads().next_power_of_two(),
                1024,
            );

            ark_std::cfg_chunks_mut!(accumulator, chunk_size)
                .enumerate()
                .for_each(|(chunk_offset, chunk)| {
                    let offset = chunk_size * chunk_offset;
                    for column in &self.0 {
                        for i in 0..chunk_size {
                            chunk[i] += column[offset + i];
                        }
                    }
                });
        }

        Matrix::new(vec![accumulator])
    }

    #[cfg(feature = "gpu")]
    pub fn sum_columns_gpu(&self) -> Matrix<F> {
        let n = self.num_rows();
        let mut accumulator = Vec::with_capacity_in(n, PageAlignedAllocator);
        accumulator.resize(n, F::zero());

        if !self.num_cols().is_zero() {
            // TODO: could improve
            let library = &PLANNER.library;
            let command_queue = &PLANNER.command_queue;
            let device = command_queue.device();
            let command_buffer = command_queue.new_command_buffer();
            let mut accumulator_buffer = buffer_mut_no_copy(device, &mut accumulator);
            let adder = AddAssignStage::<F>::new(library, n);
            for column in &self.0 {
                let column_buffer = buffer_no_copy(command_queue.device(), column);
                adder.encode(command_buffer, &mut accumulator_buffer, &column_buffer);
            }
            command_buffer.commit();
            command_buffer.wait_until_completed();
        }

        Matrix::new(vec![accumulator])
    }

    /// Sums columns into a single column matrix
    pub fn sum_columns(&self) -> Matrix<F> {
        #[cfg(not(feature = "gpu"))]
        return self.sum_columns_cpu();
        #[cfg(feature = "gpu")]
        return self.sum_columns_gpu();
    }

    pub fn commit_to_rows<D: Digest>(&self) -> MerkleTree<D> {
        let num_rows = self.num_rows();

        let mut row_hashes = vec![Default::default(); num_rows];

        #[cfg(not(feature = "parallel"))]
        let chunk_size = row_hashes.len();
        #[cfg(feature = "parallel")]
        let chunk_size = std::cmp::max(
            row_hashes.len() / rayon::current_num_threads().next_power_of_two(),
            128,
        );

        ark_std::cfg_chunks_mut!(row_hashes, chunk_size)
            .enumerate()
            .for_each(|(chunk_offset, chunk)| {
                let offset = chunk_size * chunk_offset;

                let mut row_buffer = vec![F::zero(); self.num_cols()];
                let mut row_bytes = Vec::with_capacity(row_buffer.compressed_size());

                for (i, row_hash) in chunk.iter_mut().enumerate() {
                    row_bytes.clear();
                    self.read_row(offset + i, &mut row_buffer);
                    row_buffer.serialize_compressed(&mut row_bytes).unwrap();
                    *row_hash = D::new_with_prefix(&row_bytes).finalize();
                }
            });

        MerkleTree::new(row_hashes).expect("failed to construct Merkle tree")
    }

    pub fn evaluate_at(&self, x: F) -> Vec<F> {
        ark_std::cfg_iter!(self.0)
            .map(|col| horner_evaluate(col, &x))
            .collect()
    }

    pub fn get_row(&self, row: usize) -> Option<Vec<F>> {
        if row < self.num_rows() {
            Some(self.iter().map(|col| col[row]).collect())
        } else {
            None
        }
    }

    fn read_row(&self, row_idx: usize, row: &mut [F]) {
        for (column, value) in self.0.iter().zip(row.iter_mut()) {
            *value = column[row_idx]
        }
    }

    pub fn rows(&self) -> Vec<Vec<F>> {
        (0..self.num_rows())
            .map(|row| self.get_row(row).unwrap())
            .collect()
    }

    pub fn column_degrees(&self) -> Vec<usize> {
        self.0
            .iter()
            .map(|col| {
                for i in (0..col.len()).rev() {
                    if !col[i].is_zero() {
                        return i;
                    }
                }
                0
            })
            .collect()
    }
}

impl<F: GpuField> Clone for Matrix<F> {
    fn clone(&self) -> Self {
        Self(
            self.0
                .iter()
                .map(|col| col.to_vec_in(PageAlignedAllocator))
                .collect(),
        )
    }
}

impl<F: GpuField> DerefMut for Matrix<F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<F: GpuField> Deref for Matrix<F> {
    type Target = Vec<GpuVec<F>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<F: GpuField, C: Column> Index<C> for Matrix<F> {
    type Output = GpuVec<F>;

    fn index(&self, col: C) -> &Self::Output {
        &self.0[col.index()]
    }
}

impl<F: GpuField, C: Column> IndexMut<C> for Matrix<F> {
    fn index_mut(&mut self, col: C) -> &mut Self::Output {
        &mut self.0[col.index()]
    }
}

impl<F: GpuField> TryInto<GpuVec<F>> for Matrix<F> {
    type Error = String;

    fn try_into(self) -> Result<GpuVec<F>, Self::Error> {
        match self.num_cols().cmp(&1) {
            Ordering::Equal => Ok(self.0.into_iter().next().unwrap()),
            Ordering::Greater => Err("Matrix has more than one column".to_string()),
            Ordering::Less => Err("Matrix has no columns".to_string()),
        }
    }
}

pub struct Timer<'a> {
    name: &'a str,
    start: Instant,
}

impl<'a> Timer<'a> {
    pub fn new(name: &'a str) -> Timer<'a> {
        let start = Instant::now();
        Timer { name, start }
    }
}

impl<'a> Drop for Timer<'a> {
    fn drop(&mut self) {
        println!("{} in {:?}", self.name, self.start.elapsed());
    }
}

pub fn interleave<T: Copy + Send + Sync + Default, const RADIX: usize>(
    source: &[T],
) -> Vec<[T; RADIX]> {
    let n = source.len() / RADIX;
    let mut res = vec![[T::default(); RADIX]; n];
    ark_std::cfg_iter_mut!(res)
        .enumerate()
        .for_each(|(i, element)| {
            for j in 0..RADIX {
                element[j] = source[i + j * n]
            }
        });
    res
}

// pub(crate) fn print_row<F: GpuField>(row: &[F]) {
//     for val in row {
//         print!("{val}, ");
//     }
//     println!()
// }

/// Rounds the input value up the the nearest power of two
pub fn ceil_power_of_two(value: usize) -> usize {
    if value.is_power_of_two() {
        value
    } else {
        value.next_power_of_two()
    }
}

// Evaluates the vanishing polynomial for `vanish_domain` over `eval_domain`
// E.g. evaluates `(x - v_0)(x - v_1)...(x - v_n-1)` over `eval_domain`
pub fn fill_vanishing_polynomial<F: GpuField>(
    dst: &mut [F],
    vanish_domain: &Radix2EvaluationDomain<F>,
    eval_domain: &Radix2EvaluationDomain<F>,
) {
    let n = vanish_domain.size();
    let scaled_eval_offset = eval_domain.coset_offset().pow([n as u64]);
    let scaled_eval_generator = eval_domain.group_gen().pow([n as u64]);
    let scaled_vanish_offset = vanish_domain.coset_offset_pow_size();

    #[cfg(feature = "parallel")]
    let chunk_size = std::cmp::max(n / rayon::current_num_threads(), 1024);
    #[cfg(not(feature = "parallel"))]
    let chunk_size = n;

    ark_std::cfg_chunks_mut!(dst, chunk_size)
        .enumerate()
        .for_each(|(i, chunk)| {
            let mut acc = scaled_eval_offset * scaled_eval_generator.pow([(i * chunk_size) as u64]);
            chunk.iter_mut().for_each(|coeff| {
                *coeff = acc - scaled_vanish_offset;
                acc *= &scaled_eval_generator
            })
        });
}

// taken from arkworks-rs
/// Horner's method for polynomial evaluation
pub fn horner_evaluate<F: Field>(poly_coeffs: &[F], point: &F) -> F {
    poly_coeffs
        .iter()
        .rfold(F::zero(), move |result, coeff| result * point + coeff)
}
