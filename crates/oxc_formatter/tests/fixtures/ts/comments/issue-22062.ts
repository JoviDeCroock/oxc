function x(foo: object) {
  return (
    // oxlint-disable-next-line no-unsafe-optional-chaining
    foo?.veryLongAttribute1?.veryLongAttribute2?.veryLongAttribute3
      ?.veryLongAttribute4 as any
  ).bar
}
