function MyComponent() {
  const count = signal(0);
  const double = computed(() => count.value * 2);
  return <div>{double.value}</div>;
}
