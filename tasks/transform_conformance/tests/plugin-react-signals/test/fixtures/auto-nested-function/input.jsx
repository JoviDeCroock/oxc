function MyComponent(props) {
  return props.listSignal.value.map(function iteration(x) {
    return <div>{x}</div>;
  });
}
