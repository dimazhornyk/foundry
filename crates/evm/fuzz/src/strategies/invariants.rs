use super::{fuzz_calldata_with_config, fuzz_param_from_state, CalldataFuzzDictionary};
use crate::{
    invariant::{BasicTxDetails, FuzzRunIdentifiedContracts, SenderFilters},
    strategies::{fuzz_calldata_from_state, fuzz_param, EvmFuzzState},
};
use alloy_json_abi::{Function, JsonAbi};
use alloy_primitives::{Address, Bytes};
use parking_lot::RwLock;
use proptest::prelude::*;
use std::{rc::Rc, sync::Arc};

/// Given a target address, we generate random calldata.
pub fn override_call_strat(
    fuzz_state: EvmFuzzState,
    contracts: FuzzRunIdentifiedContracts,
    target: Arc<RwLock<Address>>,
    calldata_fuzz_config: CalldataFuzzDictionary,
) -> SBoxedStrategy<(Address, Bytes)> {
    let contracts_ref = contracts.clone();

    let random_contract = any::<prop::sample::Selector>()
        .prop_map(move |selector| *selector.select(contracts_ref.lock().keys()));
    let target = any::<prop::sample::Selector>().prop_map(move |_| *target.read());

    proptest::strategy::Union::new_weighted(vec![
        (80, target.sboxed()),
        (20, random_contract.sboxed()),
    ])
    .prop_flat_map(move |target_address| {
        let fuzz_state = fuzz_state.clone();
        let calldata_fuzz_config = calldata_fuzz_config.clone();
        let (_, abi, functions) = contracts.lock().get(&target_address).unwrap().clone();
        let func = select_random_function(abi, functions);
        func.prop_flat_map(move |func| {
            fuzz_contract_with_calldata(
                fuzz_state.clone(),
                calldata_fuzz_config.clone(),
                target_address,
                func,
            )
        })
    })
    .sboxed()
}

/// Creates the invariant strategy.
///
/// Given the known and future contracts, it generates the next call by fuzzing the `caller`,
/// `calldata` and `target`. The generated data is evaluated lazily for every single call to fully
/// leverage the evolving fuzz dictionary.
///
/// The fuzzed parameters can be filtered through different methods implemented in the test
/// contract:
///
/// `targetContracts()`, `targetSenders()`, `excludeContracts()`, `targetSelectors()`
pub fn invariant_strat(
    fuzz_state: EvmFuzzState,
    senders: SenderFilters,
    contracts: FuzzRunIdentifiedContracts,
    dictionary_weight: u32,
    calldata_fuzz_config: CalldataFuzzDictionary,
) -> impl Strategy<Value = Vec<BasicTxDetails>> {
    // We only want to seed the first value, since we want to generate the rest as we mutate the
    // state
    generate_call(fuzz_state, senders, contracts, dictionary_weight, calldata_fuzz_config)
        .prop_map(|x| vec![x])
}

/// Strategy to generate a transaction where the `sender`, `target` and `calldata` are all generated
/// through specific strategies.
fn generate_call(
    fuzz_state: EvmFuzzState,
    senders: SenderFilters,
    contracts: FuzzRunIdentifiedContracts,
    dictionary_weight: u32,
    calldata_fuzz_config: CalldataFuzzDictionary,
) -> BoxedStrategy<BasicTxDetails> {
    let random_contract = select_random_contract(contracts);
    let senders = Rc::new(senders);
    random_contract
        .prop_flat_map(move |(contract, abi, functions)| {
            let func = select_random_function(abi, functions);
            let senders = senders.clone();
            let fuzz_state = fuzz_state.clone();
            let calldata_fuzz_config = calldata_fuzz_config.clone();
            func.prop_flat_map(move |func| {
                let sender =
                    select_random_sender(fuzz_state.clone(), senders.clone(), dictionary_weight);
                (
                    sender,
                    fuzz_contract_with_calldata(
                        fuzz_state.clone(),
                        calldata_fuzz_config.clone(),
                        contract,
                        func,
                    ),
                )
            })
        })
        .boxed()
}

/// Strategy to select a sender address:
/// * If `senders` is empty, then it's either a random address (10%) or from the dictionary (90%).
/// * If `senders` is not empty, a random address is chosen from the list of senders.
fn select_random_sender(
    fuzz_state: EvmFuzzState,
    senders: Rc<SenderFilters>,
    dictionary_weight: u32,
) -> BoxedStrategy<Address> {
    let senders_ref = senders.clone();
    let fuzz_strategy = proptest::strategy::Union::new_weighted(vec![
        (
            100 - dictionary_weight,
            fuzz_param(&alloy_dyn_abi::DynSolType::Address, None)
                .prop_map(move |addr| addr.as_address().unwrap())
                .boxed(),
        ),
        (
            dictionary_weight,
            fuzz_param_from_state(&alloy_dyn_abi::DynSolType::Address, fuzz_state)
                .prop_map(move |addr| addr.as_address().unwrap())
                .boxed(),
        ),
    ])
    // Too many exclusions can slow down testing.
    .prop_filter("senders not allowed", move |addr| !senders_ref.excluded.contains(addr))
    .boxed();
    if !senders.targeted.is_empty() {
        any::<prop::sample::Selector>()
            .prop_map(move |selector| *selector.select(&*senders.targeted))
            .boxed()
    } else {
        fuzz_strategy
    }
}

/// Strategy to randomly select a contract from the `contracts` list that has at least 1 function
fn select_random_contract(
    contracts: FuzzRunIdentifiedContracts,
) -> impl Strategy<Value = (Address, JsonAbi, Vec<Function>)> {
    let selectors = any::<prop::sample::Selector>();
    selectors.prop_map(move |selector| {
        let contracts = contracts.lock();
        let (addr, (_, abi, functions)) =
            selector.select(contracts.iter().filter(|(_, (_, abi, _))| !abi.functions.is_empty()));
        (*addr, abi.clone(), functions.clone())
    })
}

/// Strategy to select a random mutable function from the abi.
///
/// If `targeted_functions` is not empty, select one from it. Otherwise, take any
/// of the available abi functions.
fn select_random_function(
    abi: JsonAbi,
    targeted_functions: Vec<Function>,
) -> BoxedStrategy<Function> {
    let selectors = any::<prop::sample::Selector>();
    let possible_funcs: Vec<Function> = abi
        .functions()
        .filter(|func| {
            !matches!(
                func.state_mutability,
                alloy_json_abi::StateMutability::Pure | alloy_json_abi::StateMutability::View
            )
        })
        .cloned()
        .collect();
    let total_random = selectors.prop_map(move |selector| {
        let func = selector.select(&possible_funcs);
        func.clone()
    });
    if !targeted_functions.is_empty() {
        let selector = any::<prop::sample::Selector>()
            .prop_map(move |selector| selector.select(targeted_functions.clone()));
        selector.boxed()
    } else {
        total_random.boxed()
    }
}

/// Given a function, it returns a proptest strategy which generates valid abi-encoded calldata
/// for that function's input types.
pub fn fuzz_contract_with_calldata(
    fuzz_state: EvmFuzzState,
    calldata_fuzz_config: CalldataFuzzDictionary,
    contract: Address,
    func: Function,
) -> impl Strategy<Value = (Address, Bytes)> {
    // We need to compose all the strategies generated for each parameter in all
    // possible combinations
    // `prop_oneof!` / `TupleUnion` `Arc`s for cheap cloning
    #[allow(clippy::arc_with_non_send_sync)]
    let strats = prop_oneof![
        60 => fuzz_calldata_with_config(func.clone(), Some(calldata_fuzz_config)),
        40 => fuzz_calldata_from_state(func, fuzz_state),
    ];
    strats.prop_map(move |calldata| {
        trace!(input=?calldata);
        (contract, calldata)
    })
}
