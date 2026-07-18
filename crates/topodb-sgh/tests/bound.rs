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
