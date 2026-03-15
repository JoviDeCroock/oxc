function MyComponent(props) {
  const { signal: { value } } = props;
  return <div>{value}</div>;
}
