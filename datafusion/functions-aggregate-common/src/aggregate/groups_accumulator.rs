// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Utilities for implementing GroupsAccumulator
//! Adapter that makes [`GroupsAccumulator`] out of [`Accumulator`]

pub mod accumulate;
pub mod bool_op;
pub mod nulls;
pub mod prim_op;

use std::mem::{size_of, size_of_val};

use arrow::array::new_empty_array;
use arrow::{
    array::{ArrayRef, AsArray, BooleanArray, PrimitiveArray},
    compute,
    compute::take_arrays,
    datatypes::UInt32Type,
};
use datafusion_common::{arrow_datafusion_err, DataFusionError, Result, ScalarValue};
use datafusion_expr_common::accumulator::Accumulator;
use datafusion_expr_common::groups_accumulator::{EmitTo, GroupsAccumulator};

/// An adapter that implements [`GroupsAccumulator`] for any [`Accumulator`]
///
/// While [`Accumulator`] are simpler to implement and can support
/// more general calculations (like retractable window functions),
/// they are not as fast as a specialized `GroupsAccumulator`. This
/// interface bridges the gap so the group by operator only operates
/// in terms of [`Accumulator`].
///
/// Internally, this adapter creates a new [`Accumulator`] for each group which
/// stores the state for that group. This both requires an allocation for each
/// Accumulator, internal indices, as well as whatever internal allocations the
/// Accumulator itself requires.
///
/// For example, a `MinAccumulator` that computes the minimum string value with
/// a [`ScalarValue::Utf8`]. That will require at least two allocations per group
/// (one for the `MinAccumulator` and one for the `ScalarValue::Utf8`).
///
/// ```text
///                       ┌─────────────────────────────────┐
///                       │MinAccumulator {                 │
///                ┌─────▶│ min: ScalarValue::Utf8("A")     │───────┐
///                │      │}                                │       │
///                │      └─────────────────────────────────┘       └───────▶   "A"
///    ┌─────┐     │      ┌─────────────────────────────────┐
///    │  0  │─────┘      │MinAccumulator {                 │
///    ├─────┤     ┌─────▶│ min: ScalarValue::Utf8("Z")     │───────────────▶   "Z"
///    │  1  │─────┘      │}                                │
///    └─────┘            └─────────────────────────────────┘                   ...
///      ...                 ...
///    ┌─────┐            ┌────────────────────────────────┐
///    │ N-2 │            │MinAccumulator {                │
///    ├─────┤            │  min: ScalarValue::Utf8("A")   │────────────────▶   "A"
///    │ N-1 │─────┐      │}                               │
///    └─────┘     │      └────────────────────────────────┘
///                │      ┌────────────────────────────────┐        ┌───────▶   "Q"
///                │      │MinAccumulator {                │        │
///                └─────▶│  min: ScalarValue::Utf8("Q")   │────────┘
///                       │}                               │
///                       └────────────────────────────────┘
///
///
///  Logical group         Current Min/Max value for that group stored
///     number             as a ScalarValue which points to an
///                        individually allocated String
///
///```
///
/// # Optimizations
///
/// The adapter minimizes the number of calls to [`Accumulator::update_batch`]
/// by first collecting the input rows for each group into a contiguous array
/// using [`compute::take`]
///
pub struct GroupsAccumulatorAdapter {
    factory: Box<dyn Fn() -> Result<Box<dyn Accumulator>> + Send>,

    /// state for each group, stored in group_index order
    states: Vec<AccumulatorState>,

    /// Current memory usage, in bytes.
    ///
    /// Note this is incrementally updated with deltas to avoid the
    /// call to size() being a bottleneck. We saw size() being a
    /// bottleneck in earlier implementations when there were many
    /// distinct groups.
    allocation_bytes: usize,
}

struct AccumulatorState {
    /// [`Accumulator`] that stores the per-group state
    accumulator: Box<dyn Accumulator>,

    /// scratch space: indexes in the input array that will be fed to
    /// this accumulator. Stores indexes as `u32` to match the arrow
    /// `take` kernel input.
    indices: Vec<u32>,
}

impl AccumulatorState {
    fn new(accumulator: Box<dyn Accumulator>) -> Self {
        Self {
            accumulator,
            indices: vec![],
        }
    }

    /// Returns the amount of memory taken by this structure and its accumulator
    fn size(&self) -> usize {
        self.accumulator.size() + size_of_val(self) + self.indices.allocated_size()
    }
}

impl GroupsAccumulatorAdapter {
    /// Create a new adapter that will create a new [`Accumulator`]
    /// for each group, using the specified factory function
    pub fn new<F>(factory: F) -> Self
    where
        F: Fn() -> Result<Box<dyn Accumulator>> + Send + 'static,
    {
        Self {
            factory: Box::new(factory),
            states: vec![],
            allocation_bytes: 0,
        }
    }

    /// Ensure that self.accumulators has total_num_groups
    fn make_accumulators_if_needed(&mut self, total_num_groups: usize) -> Result<()> {
        // can't shrink
        assert!(total_num_groups >= self.states.len());
        let vec_size_pre = self.states.allocated_size();

        // instantiate new accumulators
        let new_accumulators = total_num_groups - self.states.len();
        for _ in 0..new_accumulators {
            let accumulator = (self.factory)()?;
            let state = AccumulatorState::new(accumulator);
            self.add_allocation(state.size());
            self.states.push(state);
        }

        self.adjust_allocation(vec_size_pre, self.states.allocated_size());
        Ok(())
    }

    /// invokes f(accumulator, values) for each group that has values
    /// in group_indices.
    ///
    /// This function first reorders the input and filter so that
    /// values for each group_index are contiguous and then invokes f
    /// on the contiguous ranges, to minimize per-row overhead
    ///
    /// ```text
    /// ┌─────────┐   ┌─────────┐   ┌ ─ ─ ─ ─ ┐                       ┌─────────┐   ┌ ─ ─ ─ ─ ┐
    /// │ ┌─────┐ │   │ ┌─────┐ │     ┌─────┐              ┏━━━━━┓    │ ┌─────┐ │     ┌─────┐
    /// │ │  2  │ │   │ │ 200 │ │   │ │  t  │ │            ┃  0  ┃    │ │ 200 │ │   │ │  t  │ │
    /// │ ├─────┤ │   │ ├─────┤ │     ├─────┤              ┣━━━━━┫    │ ├─────┤ │     ├─────┤
    /// │ │  2  │ │   │ │ 100 │ │   │ │  f  │ │            ┃  0  ┃    │ │ 300 │ │   │ │  t  │ │
    /// │ ├─────┤ │   │ ├─────┤ │     ├─────┤              ┣━━━━━┫    │ ├─────┤ │     ├─────┤
    /// │ │  0  │ │   │ │ 200 │ │   │ │  t  │ │            ┃  1  ┃    │ │ 200 │ │   │ │NULL │ │
    /// │ ├─────┤ │   │ ├─────┤ │     ├─────┤   ────────▶  ┣━━━━━┫    │ ├─────┤ │     ├─────┤
    /// │ │  1  │ │   │ │ 200 │ │   │ │NULL │ │            ┃  2  ┃    │ │ 200 │ │   │ │  t  │ │
    /// │ ├─────┤ │   │ ├─────┤ │     ├─────┤              ┣━━━━━┫    │ ├─────┤ │     ├─────┤
    /// │ │  0  │ │   │ │ 300 │ │   │ │  t  │ │            ┃  2  ┃    │ │ 100 │ │   │ │  f  │ │
    /// │ └─────┘ │   │ └─────┘ │     └─────┘              ┗━━━━━┛    │ └─────┘ │     └─────┘
    /// └─────────┘   └─────────┘   └ ─ ─ ─ ─ ┘                       └─────────┘   └ ─ ─ ─ ─ ┘
    ///
    /// logical group   values      opt_filter           logical group  values       opt_filter
    ///
    /// ```
    fn invoke_per_accumulator<F>(
        &mut self,
        values: &[ArrayRef],
        group_indices: &[usize],
        opt_filter: Option<&BooleanArray>,
        total_num_groups: usize,
        f: F,
    ) -> Result<()>
    where
        F: Fn(&mut dyn Accumulator, &[ArrayRef]) -> Result<()>,
    {
        self.make_accumulators_if_needed(total_num_groups)?;

        assert_eq!(values[0].len(), group_indices.len());

        // figure out which input rows correspond to which groups.
        // Note that self.state.indices starts empty for all groups
        // (it is cleared out below)
        for (idx, group_index) in group_indices.iter().enumerate() {
            self.states[*group_index].indices.push(idx as u32);
        }

        // groups_with_rows holds a list of group indexes that have
        // any rows that need to be accumulated, stored in order of
        // group_index

        let mut groups_with_rows = vec![];

        // batch_indices holds indices into values, each group is contiguous
        let mut batch_indices = vec![];

        // offsets[i] is index into batch_indices where the rows for
        // group_index i starts
        let mut offsets = vec![0];

        let mut offset_so_far = 0;
        for (group_index, state) in self.states.iter_mut().enumerate() {
            let indices = &state.indices;
            if indices.is_empty() {
                continue;
            }

            groups_with_rows.push(group_index);
            batch_indices.extend_from_slice(indices);
            offset_so_far += indices.len();
            offsets.push(offset_so_far);
        }
        let batch_indices = batch_indices.into();

        // reorder the values and opt_filter by batch_indices so that
        // all values for each group are contiguous, then invoke the
        // accumulator once per group with values
        let values = take_arrays(values, &batch_indices, None)?;
        let opt_filter = get_filter_at_indices(opt_filter, &batch_indices)?;

        // invoke each accumulator with the appropriate rows, first
        // pulling the input arguments for this group into their own
        // RecordBatch(es)
        let iter = groups_with_rows.iter().zip(offsets.windows(2));

        let mut sizes_pre = 0;
        let mut sizes_post = 0;
        for (&group_idx, offsets) in iter {
            let state = &mut self.states[group_idx];
            sizes_pre += state.size();

            let values_to_accumulate = slice_and_maybe_filter(
                &values,
                opt_filter.as_ref().map(|f| f.as_boolean()),
                offsets,
            )?;
            f(state.accumulator.as_mut(), &values_to_accumulate)?;

            // clear out the state so they are empty for next
            // iteration
            state.indices.clear();
            sizes_post += state.size();
        }

        self.adjust_allocation(sizes_pre, sizes_post);
        Ok(())
    }

    /// Increment the allocation by `n`
    ///
    /// See [`Self::allocation_bytes`] for rationale.
    fn add_allocation(&mut self, size: usize) {
        self.allocation_bytes += size;
    }

    /// Decrease the allocation by `n`
    ///
    /// See [`Self::allocation_bytes`] for rationale.
    fn free_allocation(&mut self, size: usize) {
        // use saturating sub to avoid errors if the accumulators
        // report erroneous sizes
        self.allocation_bytes = self.allocation_bytes.saturating_sub(size)
    }

    /// Adjusts the allocation for something that started with
    /// start_size and now has new_size avoiding overflow
    ///
    /// See [`Self::allocation_bytes`] for rationale.
    fn adjust_allocation(&mut self, old_size: usize, new_size: usize) {
        if new_size > old_size {
            self.add_allocation(new_size - old_size)
        } else {
            self.free_allocation(old_size - new_size)
        }
    }
}

impl GroupsAccumulator for GroupsAccumulatorAdapter {
    fn update_batch(
        &mut self,
        values: &[ArrayRef],
        group_indices: &[usize],
        opt_filter: Option<&BooleanArray>,
        total_num_groups: usize,
    ) -> Result<()> {
        self.invoke_per_accumulator(
            values,
            group_indices,
            opt_filter,
            total_num_groups,
            |accumulator, values_to_accumulate| {
                accumulator.update_batch(values_to_accumulate)
            },
        )?;
        Ok(())
    }

    fn evaluate(&mut self, emit_to: EmitTo) -> Result<ArrayRef> {
        let vec_size_pre = self.states.allocated_size();

        let states = emit_to.take_needed(&mut self.states);

        let results: Vec<ScalarValue> = states
            .into_iter()
            .map(|mut state| {
                self.free_allocation(state.size());
                state.accumulator.evaluate()
            })
            .collect::<Result<_>>()?;

        let result = ScalarValue::iter_to_array(results);

        self.adjust_allocation(vec_size_pre, self.states.allocated_size());

        result
    }

    // filtered_null_mask(opt_filter, &values);
    fn state(&mut self, emit_to: EmitTo) -> Result<Vec<ArrayRef>> {
        let vec_size_pre = self.states.allocated_size();
        let states = emit_to.take_needed(&mut self.states);

        // each accumulator produces a potential vector of values
        // which we need to form into columns
        let mut results: Vec<Vec<ScalarValue>> = vec![];

        for mut state in states {
            self.free_allocation(state.size());
            let accumulator_state = state.accumulator.state()?;
            results.resize_with(accumulator_state.len(), Vec::new);
            for (idx, state_val) in accumulator_state.into_iter().enumerate() {
                results[idx].push(state_val);
            }
        }

        // create an array for each intermediate column
        let arrays = results
            .into_iter()
            .map(ScalarValue::iter_to_array)
            .collect::<Result<Vec<_>>>()?;

        // double check each array has the same length (aka the
        // accumulator was implemented correctly
        if let Some(first_col) = arrays.first() {
            for arr in &arrays {
                assert_eq!(arr.len(), first_col.len())
            }
        }
        self.adjust_allocation(vec_size_pre, self.states.allocated_size());

        Ok(arrays)
    }

    fn merge_batch(
        &mut self,
        values: &[ArrayRef],
        group_indices: &[usize],
        opt_filter: Option<&BooleanArray>,
        total_num_groups: usize,
    ) -> Result<()> {
        self.invoke_per_accumulator(
            values,
            group_indices,
            opt_filter,
            total_num_groups,
            |accumulator, values_to_accumulate| {
                accumulator.merge_batch(values_to_accumulate)?;
                Ok(())
            },
        )?;
        Ok(())
    }

    fn size(&self) -> usize {
        self.allocation_bytes
    }

    fn convert_to_state(
        &self,
        values: &[ArrayRef],
        opt_filter: Option<&BooleanArray>,
    ) -> Result<Vec<ArrayRef>> {
        let num_rows = values[0].len();

        // If there are no rows, return empty arrays
        if num_rows == 0 {
            // create empty accumulator to get the state types
            let empty_state = (self.factory)()?.state()?;
            let empty_arrays = empty_state
                .into_iter()
                .map(|state_val| new_empty_array(&state_val.data_type()))
                .collect::<Vec<_>>();

            return Ok(empty_arrays);
        }

        // Each row has its respective group
        let mut results = vec![];
        for row_idx in 0..num_rows {
            // Create the empty accumulator for converting
            let mut converted_accumulator = (self.factory)()?;

            // Convert row to states
            let values_to_accumulate =
                slice_and_maybe_filter(values, opt_filter, &[row_idx, row_idx + 1])?;
            converted_accumulator.update_batch(&values_to_accumulate)?;
            let states = converted_accumulator.state()?;

            // Resize results to have enough columns according to the converted states
            results.resize_with(states.len(), || Vec::with_capacity(num_rows));

            // Add the states to results
            for (idx, state_val) in states.into_iter().enumerate() {
                results[idx].push(state_val);
            }
        }

        let arrays = results
            .into_iter()
            .map(ScalarValue::iter_to_array)
            .collect::<Result<Vec<_>>>()?;

        Ok(arrays)
    }

    fn supports_convert_to_state(&self) -> bool {
        true
    }
}

/// Extension trait for [`Vec`] to account for allocations.
pub trait VecAllocExt {
    /// Item type.
    type T;
    /// Return the amount of memory allocated by this Vec (not
    /// recursively counting any heap allocations contained within the
    /// structure). Does not include the size of `self`
    fn allocated_size(&self) -> usize;
}

impl<T> VecAllocExt for Vec<T> {
    type T = T;
    fn allocated_size(&self) -> usize {
        size_of::<T>() * self.capacity()
    }
}

fn get_filter_at_indices(
    opt_filter: Option<&BooleanArray>,
    indices: &PrimitiveArray<UInt32Type>,
) -> Result<Option<ArrayRef>> {
    opt_filter
        .map(|filter| {
            compute::take(
                &filter, indices, None, // None: no index check
            )
        })
        .transpose()
        .map_err(|e| arrow_datafusion_err!(e))
}

// Copied from physical-plan
pub(crate) fn slice_and_maybe_filter(
    aggr_array: &[ArrayRef],
    filter_opt: Option<&BooleanArray>,
    offsets: &[usize],
) -> Result<Vec<ArrayRef>> {
    let (offset, length) = (offsets[0], offsets[1] - offsets[0]);
    let sliced_arrays: Vec<ArrayRef> = aggr_array
        .iter()
        .map(|array| array.slice(offset, length))
        .collect();

    if let Some(f) = filter_opt {
        let filter = f.slice(offset, length);

        sliced_arrays
            .iter()
            .map(|array| {
                compute::filter(&array, &filter).map_err(|e| arrow_datafusion_err!(e))
            })
            .collect()
    } else {
        Ok(sliced_arrays)
    }
}
