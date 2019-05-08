//! Multi-way equijoin expression plan.
//!
//! This plan provides us the opportunity to map out a non-trivial differential
//! implementation for a complex join query. In particular, we are able to invoke
//! delta-query and worst-case optimal join plans, which avoid any intermediate
//! materialization.
//!
//! Each `MultiwayJoin` indicates several source collections, equality constraints
//! among their attributes, and then the set of attributes to produce as results.
//!
//! One naive implementation would take each input collection in order, and develop
//! the join restricted to the prefix of relations so far. Ideally the order would
//! be such that joined collections have equality constraints and prevent Cartesian
//! explosion. At each step, a new collection picks out some of the attributes and
//! instantiates a primitive binary join between the accumulated collection and the
//! next collection.
//!
//! A more sophisticated implementation establishes delta queries for each input
//! collection, which responds to changes in that input collection against the
//! current other input collections. For each input collection we may choose very
//! different join orders, as the order must follow equality constraints.
//!
//! A further implementation could develop the results attribute-by-attribute, as
//! opposed to collection-by-collection, which gives us the ability to use column
//! indices rather than whole-collection indices.

use std::hash::Hash;

use timely::dataflow::Scope;

use differential_dataflow::operators::arrange::{ArrangeBySelf, ArrangeByKey};

use differential_dataflow::{Collection, ExchangeData};
use plan::{Plan, Render};
use {TraceManager, Time, Diff};

/// A multiway join of muliple relations.
///
/// By expressing multiple relations and required equivalances between their attributes,
/// we can more efficiently design incremental update strategies without materializing
/// and indexing intermediate relations.
#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MultiwayJoin<Value> {
    /// A list of (attribute index, input) pairs to extract.
    pub results: Vec<(usize, usize)>,
    /// A list of source collections.
    pub sources: Vec<Box<Plan<Value>>>,
    /// Equality constraints.
    ///
    /// Equality constraints are presented as lists of `(attr, input)` equivalence classes.
    /// This means that each `(attr, input)` pair can exist in at most one list; if it would
    /// appear in more than one list, those two lists should be merged.
    pub equalities: Vec<Vec<(usize, usize)>>,
}

impl<V: ExchangeData+Hash> Render for MultiwayJoin<V> {

    type Value = V;

    fn render<S: Scope<Timestamp = Time>>(
        &self,
        scope: &mut S,
        arrangements: &mut TraceManager<Self::Value>) -> Collection<S, Vec<Self::Value>, Diff>
    {
        // The plan here is that each stream of changes comes in, and has a delta query.
        // For each each stream, we need to work through each other relation ensuring that
        // each new relation we add has some attributes in common with the developing set.

        // Attributes we may need from any and all relations.
        let mut relevant_attributes = Vec::new();
        relevant_attributes.extend(self.results.iter().cloned());
        relevant_attributes.extend(self.equalities.iter().flat_map(|list| list.iter().cloned()));
        relevant_attributes.sort();
        relevant_attributes.dedup();

        // Into which we accumulate change streams.
        let mut accumulated_changes = Vec::new();

        // For each participating relation, we build a delta query dataflow.
        for (index, plan) in self.sources.iter().enumerate() {

            // Restrict down to relevant attributes.
            let mut attributes: Vec<(usize, usize)> =
            relevant_attributes
                .iter()
                .filter(|(_attr, input)| input == &index)
                .cloned()
                .collect::<Vec<_>>();

            let attributes_init = attributes.clone();

            // Ensure the plan is rendered and cached.
            if arrangements.get_unkeyed(&plan).is_none() {
                let collection = plan.render(scope, arrangements);
                arrangements.set_unkeyed(plan, &collection.arrange_by_self().trace);
            }
            let changes =
            arrangements
                .get_unkeyed(&plan)
                .expect("Surely we just ensured this")
                .import(scope)
                .as_collection(|val,&()| val.clone())
                .map(move |tuple| attributes_init.iter().map(|&(attr,_)|
                        tuple[attr].clone()).collect::<Vec<_>>()
                );

            // Before constructing the dataflow, which takes a borrow on `scope`,
            // we'll want to ensure that we have all of the necessary data assets
            // in place. This requires a bit of planning first, then the building.

            // Acquire a sane sequence in which to join the relations:
            //
            // This is a sequence of relation identifiers, starting with `index`,
            // such that each has at least one attribute in common with a prior
            // relation, and so can be effectively joined.
            let join_order = plan_join_order(index, &self.equalities);
            let mut join_plan = Vec::new();

            // Skipping `index`, join in each relation in sequence.
            for join_idx in join_order.into_iter().skip(1) {

                // To join a relation, we need to determine any constraints on
                // attributes in common with prior relations. Any other values
                // should be appended to tuples in `changes` with care taken to
                // update `attributes`.
                let (keys, priors) = determine_keys_priors(join_idx, &self.equalities, &attributes[..]);

                // The fields in `sources[join_idx]` that should be values are those
                // that are required output or participate in an equality constraint,
                // but *WHICH ARE NOT* in `keys`.
                let vals =
                relevant_attributes
                    .iter()
                    .filter(|&(attr,index)| index == &join_idx && !keys.contains(&attr))
                    .cloned()
                    .collect::<Vec<_>>();

                let mut projection = Vec::new();
                for &attr in keys.iter() {
                    projection.push(attr);
                }
                for &(attr, _index) in vals.iter() {
                    projection.push(attr);
                }
                // TODO: Sort, to improve chances of re-use opportunities.
                //       Requires understanding how attributes move to get the right
                //       key selectors out though.
                // projection.sort();
                // projection.dedup(); // Should already be deduplicated, probably?

                // Get a plan for the projection on to these few attributes.
                let plan = self.sources[join_idx].clone().project(projection);

                if arrangements.get_keyed(&plan, &keys[..]).is_none() {
                    let keys_clone = keys.clone();
                    let arrangement =
                    plan.render(scope, arrangements)
                        .map(move |tuple| (keys_clone.iter().map(|&i| tuple[i].clone()).collect::<Vec<_>>(), tuple))
                        .arrange_by_key();

                    arrangements.set_keyed(&plan, &keys[..], &arrangement.trace);
                }

                let arrangement =
                arrangements
                    .get_keyed(&plan, &keys[..])
                    .expect("Surely we just ensured this");

                let key_selector = std::rc::Rc::new(move |change: &Vec<V>|
                    priors.iter().map(|&p| change[p].clone()).collect::<Vec<_>>()
                );

                join_plan.push((join_idx, key_selector, arrangement));

                attributes.extend(vals.into_iter());
            }

            // Build the dataflow.
            use dogsdogsdogs::altneu::AltNeu;

            let scope_name = format!("DeltaRule: {}/{}", index, self.sources.len());
            let changes = scope.clone().scoped::<AltNeu<_>,_,_>(&scope_name, |inner| {

                // This should default to an `AltNeu::Alt` timestamp.
                let mut changes = changes.enter(inner);

                for (join_idx, key_selector, mut trace) in join_plan.into_iter() {

                    // Use alt or neu timestamps based on relative indices.
                    // Must have an `if` statement here as the two arrangement have different
                    // types, and we would to determine `alt` v `neu` once, rather than per
                    // tuple in the cursor.
                    changes =
                    if join_idx < index {
                        let arrangement = trace.import(scope).enter_at(inner, |_,_,t| AltNeu::alt(t.clone()));
                        dogsdogsdogs::operators::propose(&changes, arrangement, key_selector)
                    }
                    else {
                        let arrangement = trace.import(scope).enter_at(inner, |_,_,t| AltNeu::neu(t.clone()));
                        dogsdogsdogs::operators::propose(&changes, arrangement, key_selector)
                    }
                    .map(|(mut prefix, extensions)| { prefix.extend(extensions.into_iter()); prefix });
                }

                // Extract `self.results` in order, using `attributes`.
                let extract_map =
                self.results
                    .iter()
                    .map(move |x| attributes.iter().position(|i| i == x).expect("Output attribute not found!"))
                    .collect::<Vec<_>>();

                changes
                    .map(move |tuple| extract_map.iter().map(|&i| tuple[i].clone()).collect::<Vec<_>>())
                    .leave()
            });

            accumulated_changes.push(changes);
        }

        differential_dataflow::collection::concatenate(scope, accumulated_changes.into_iter())
    }
}

/// Sequences relations in `constraints`.
///
/// Relations become available for sequencing as soon as they share a constraint with
/// either `source` or another sequenced relation.
fn plan_join_order(source: usize, constraints: &[Vec<(usize, usize)>]) -> Vec<usize> {

    let mut result = vec![source];
    let mut active = true;
    while active {
        active = false;
        for constraint in constraints.iter() {
            // Check to see if the constraint contains a sequenced relation.
            if constraint.iter().any(|(_,index)| result.contains(index)) {
                // If so, sequence any unsequenced relations.
                for (_, index) in constraint.iter() {
                    if !result.contains(index) {
                        result.push(*index);
                        active = true;
                    }
                }
            }
        }
    }

    result
}

/// Identifies keys and values for a join.
///
/// The result is a sequence, for each
fn determine_keys_priors(
    relation: usize,
    constraints: &[Vec<(usize, usize)>],
    current_attributes: &[(usize, usize)],
)
-> (Vec<usize>, Vec<usize>)
{
    // The fields in `sources[join_idx]` that should be keys are those
    // that share an equality constraint with an element of `attributes`.
    // For each key, we should capture the associated `attributes` entry
    // so that we can easily prepare the keys of the `delta` stream.
    let mut keys = Vec::new();
    let mut priors = Vec::new();
    for constraint in constraints.iter() {

        // If there is an intersection between `constraint` and `current_attributes`,
        // we should capture the position in `current_attributes` and emit all of the
        // attributes for `relation`.
        if let Some(prior) = current_attributes.iter().position(|x| constraint.contains(x)) {
            for &(attr, index) in constraint.iter() {
                if index == relation {
                    keys.push(attr);
                    priors.push(prior);
                }
            }
        }
    }

    (keys, priors)
}
