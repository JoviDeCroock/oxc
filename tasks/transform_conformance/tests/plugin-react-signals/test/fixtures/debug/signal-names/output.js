import { useSignals as _useSignals } from '@preact/signals-react/runtime';
function MyComponent() {
	var _effect = _useSignals(1, 'MyComponent');
	try {
		const count = signal(0, { name: 'count (input.jsx:2)' });
		const double = computed(() => count.value * 2, { name: 'double (input.jsx:3)' });
		return <div>{double.value}</div>;
	} finally {
		_effect.f();
	}
}
