import { useSignals as _useSignals } from '@preact/signals-react/runtime';
function MyComponent(props) {
	var _effect = _useSignals(1);
	try {
		const { signal: { value } } = props;
		return <div>{value}</div>;
	} finally {
		_effect.f();
	}
}
