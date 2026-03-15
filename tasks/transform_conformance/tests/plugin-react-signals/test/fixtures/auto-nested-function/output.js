import { useSignals as _useSignals } from '@preact/signals-react/runtime';
function MyComponent(props) {
	var _effect = _useSignals(1);
	try {
		return props.listSignal.value.map(function iteration(x) {
			return <div>{x}</div>;
		});
	} finally {
		_effect.f();
	}
}
