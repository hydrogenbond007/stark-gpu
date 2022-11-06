use crate::constraint::Element;
use crate::constraint::Term;
use crate::merkle::MerkleTree;
use crate::utils::horner_evaluate;
use crate::Column;
use crate::Constraint;
use ark_ff::Field;
use ark_ff::Zero;
use ark_poly::domain::Radix2EvaluationDomain;
use ark_poly::EvaluationDomain;
use ark_serialize::CanonicalSerialize;
use digest::Digest;
use gpu_poly::prelude::*;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use std::cmp::Ordering;
use std::ops::Add;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ops::Index;
use std::ops::IndexMut;
use std::ops::Mul;
use std::ops::MulAssign;
use std::process::Output;

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
    fn into_polynomials_gpu(mut self, domain: Radix2EvaluationDomain<F::FftField>) -> Self {
        let mut ifft = GpuIfft::from(domain);

        for column in &mut self.0 {
            ifft.encode(column);
        }

        ifft.execute();

        self
    }

    #[cfg(not(feature = "gpu"))]
    fn into_polynomials_cpu(mut self, domain: Radix2EvaluationDomain<F::FftField>) -> Self {
        self.0.iter_mut().for_each(|col| domain.ifft_in_place(col));
        self
    }

    /// Interpolates the columns of the polynomials over the domain
    pub fn into_polynomials(self, domain: Radix2EvaluationDomain<F::FftField>) -> Self {
        #[cfg(not(feature = "gpu"))]
        return self.into_polynomials_cpu(domain);
        #[cfg(feature = "gpu")]
        return self.into_polynomials_gpu(domain);
    }

    /// Interpolates the columns of the matrix over the domain
    pub fn interpolate(&self, domain: Radix2EvaluationDomain<F::FftField>) -> Self {
        self.clone().into_polynomials(domain)
    }

    #[cfg(not(feature = "gpu"))]
    fn into_evaluations_cpu(mut self, domain: Radix2EvaluationDomain<F::FftField>) -> Self {
        for column in &mut self.0 {
            domain.fft_in_place(column);
        }
        self
    }

    #[cfg(feature = "gpu")]
    fn into_evaluations_gpu(mut self, domain: Radix2EvaluationDomain<F::FftField>) -> Self {
        let mut fft = GpuFft::from(domain);

        for column in &mut self.0 {
            fft.encode(column);
        }

        fft.execute();

        self
    }

    /// Evaluates the columns of the matrix
    pub fn into_evaluations(self, domain: Radix2EvaluationDomain<F::FftField>) -> Self {
        #[cfg(not(feature = "gpu"))]
        return self.into_evaluations_cpu(domain);
        #[cfg(feature = "gpu")]
        return self.into_evaluations_gpu(domain);
    }

    /// Evaluates the columns of the matrix
    pub fn evaluate(&self, domain: Radix2EvaluationDomain<F::FftField>) -> Self {
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

    pub fn evaluate_at<T: Field>(&self, x: T) -> Vec<T>
    where
        T: for<'a> Add<&'a F, Output = T>,
    {
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

enum Col<'a, Fp, Fq> {
    Fp(&'a GpuVec<Fp>),
    Fq(&'a GpuVec<Fq>),
}

pub enum GroupItem<'a, Fp, Fq> {
    Fp(&'a Matrix<Fp>),
    Fq(&'a Matrix<Fq>),
}

macro_rules! map {
    ($self:expr, $f1:ident $(, $x:expr)*) => {
        match $self {
            GroupItem::Fp(matrix) => matrix.$f1($($x)*),
            GroupItem::Fq(matrix) => matrix.$f1($($x)*),
        }
    }
}

pub struct MatrixGroup<'a, Fp, Fq = Fp>(Vec<GroupItem<'a, Fp, Fq>>);

impl<'a, Fp: GpuField, Fq: GpuField> MatrixGroup<'a, Fp, Fq> {
    fn new(items: Vec<GroupItem<'a, Fp, Fq>>) -> Self {
        MatrixGroup(items)
    }

    fn num_rows(&self) -> usize {
        if self.0.is_empty() {
            return 0;
        }

        // Check all matricies have the same number of rows
        let expected = map!(self.0[0], num_rows);
        assert!(self.0.iter().all(|m| map!(m, num_rows) == expected));
        expected
    }

    fn get_column(&self, index: usize) -> Col<'a, Fp, Fq> {
        todo!()
    }
}

impl<'a, Fp: GpuField, Fq: GpuField> MatrixGroup<'a, Fp, Fq>
where
    Fq: for<'b> MulAssign<&'b Fp>,
{
    #[cfg(feature = "gpu")]
    fn evaluate_symbolic_gpu(
        &self,
        results: &mut [GpuVec<Fq>],
        constraints: &[Constraint<Fq>],
        step: usize,
    ) {
        todo!()
    }

    #[cfg(not(feature = "gpu"))]
    fn evaluate_symbolic_cpu(
        &self,
        results: &mut [GpuVec<Fq>],
        constraints: &[Constraint<Fq>],
        step: usize,
    ) {
        let n = self.num_rows();
        #[cfg(not(feature = "parallel"))]
        let chunk_size = n;
        #[cfg(feature = "parallel")]
        let chunk_size = std::cmp::max(n / rayon::current_num_threads().next_power_of_two(), 1024);

        for (result, constraint) in results.iter_mut().zip(constraints) {
            ark_std::cfg_chunks_mut!(result, chunk_size)
                .enumerate()
                .for_each(|(chunk_offset, chunk)| {
                    let offset = chunk_offset * chunk_size;

                    let mut scratch_fp = Vec::with_capacity(chunk.len());
                    scratch_fp.resize(chunk.len(), Fp::zero());

                    let mut scratch_fq = Vec::with_capacity(chunk.len());
                    scratch_fq.resize(chunk.len(), Fq::zero());

                    for (i, Term(coeff, variables)) in constraint.0.iter().enumerate() {
                        scratch_fp.fill(Fp::one());
                        scratch_fq.fill(*coeff);
                        for (element, power) in &variables.0 {
                            let (col_index, shift) = match element {
                                Element::Curr(col_index) => (col_index, 0),
                                Element::Next(col_index) => (col_index, step),
                                _ => unreachable!(),
                            };

                            // TODO: map like macro could help here
                            match self.get_column(*col_index) {
                                Col::Fp(col) => {
                                    for (i, scratch) in scratch_fp.iter_mut().enumerate() {
                                        *scratch *=
                                            col[(offset + shift + i) % n].pow([*power as u64])
                                    }
                                }
                                Col::Fq(col) => {
                                    for (i, scratch) in scratch_fq.iter_mut().enumerate() {
                                        *scratch *=
                                            col[(offset + shift + i) % n].pow([*power as u64])
                                    }
                                }
                            }
                        }
                        scratch_fq
                            .iter_mut()
                            .zip(&scratch_fp)
                            .for_each(|(s_fq, s_fp)| *s_fq *= s_fp);
                        if i == 0 {
                            for (result, scratch) in chunk.iter_mut().zip(&scratch_fq) {
                                *result = *scratch;
                            }
                        } else {
                            for (result, scratch) in chunk.iter_mut().zip(&scratch_fq) {
                                *result += scratch;
                            }
                        }
                    }
                });
        }
    }

    // TODO: step is related to constraints. Needs refactor
    fn evaluate_symbolic(
        &self,
        constraints: &[Constraint<Fq>],
        challenges: &[Fq],
        step: usize,
    ) -> Matrix<Fq> {
        let n = self.num_rows();
        let constraints_without_challenges: Vec<Constraint<Fq>> = constraints
            .iter()
            .map(|c| c.evaluate_challenges(challenges))
            .collect();
        if constraints_without_challenges.is_empty() {
            return Matrix::new(vec![]);
        }

        let mut results = Matrix::new(
            constraints
                .iter()
                .map(|_| {
                    let mut col = Vec::with_capacity_in(n, PageAlignedAllocator);
                    col.resize(n, Fq::zero());
                    col
                })
                .collect(),
        );

        #[cfg(feature = "gpu")]
        self.evaluate_symbolic_gpu(&mut results.0, &constraints_without_challenges, step);
        #[cfg(not(feature = "gpu"))]
        self.evaluate_symbolic_cpu(&mut results, &constraints_without_challenges, step);

        results
    }
}
