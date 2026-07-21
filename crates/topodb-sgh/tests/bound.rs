use topodb_sgh::schema::bound::worst_case;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;

#[test]
fn computes_bound_for_the_simple_fixture() {
    // survey: agent, retries 2, repairs 1 -> 1 + 2 + 2*1 = 5 model calls
    // build:  command, retries 1, repairs 0 -> 1 + 1 + 0 = 2 command runs, 0 model calls
    let g = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let v = validate(&g).unwrap();
    let b = worst_case(&v);
    assert_eq!(b.agent_calls, 5);
    assert_eq!(b.command_runs, 2);
}

#[test]
fn zero_budget_agent_costs_exactly_one_call() {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    assert_eq!(worst_case(&v).agent_calls, 1);
}

#[test]
fn bound_sums_across_a_mixed_graph_and_gates_cost_nothing() {
    // The bound is a sum over nodes, agent and command dimensions kept
    // separate. Verify the arithmetic on a graph that exercises both budget
    // terms at once, plus a gate (which must contribute nothing).
    //
    //   agent  A: retries 2, repairs 1 -> 1 + 2 + 2*1 = 5 model calls
    //   agent  B: retries 0, repairs 3 -> 1 + 0 + 2*3 = 7 model calls
    //   command C: retries 4           -> 1 + 4       = 5 command runs
    //   command D: retries 0           -> 1           = 1 command run
    //   gate   G:                      -> 0 of either
    //   => agent_calls = 12, command_runs = 6
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 2, repairs: 1}}\n\
         - {id: b, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 3}}\n\
         - {id: c, kind: command, run: 'true', needs: [b], budget: {retries: 4, repairs: 0}}\n\
         - {id: d, kind: command, run: 'true', needs: [b], budget: {retries: 0, repairs: 0}}\n\
         - {id: g, kind: gate, needs: [c, d], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let b = worst_case(&v);
    assert_eq!(b.agent_calls, 12, "5 (A) + 7 (B), gate adds nothing");
    assert_eq!(b.command_runs, 6, "5 (C) + 1 (D), gate adds nothing");
}

#[test]
fn a_lone_gate_has_a_zero_bound() {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: g, kind: gate, budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let b = worst_case(&v);
    assert_eq!(b.agent_calls, 0);
    assert_eq!(b.command_runs, 0);
}
