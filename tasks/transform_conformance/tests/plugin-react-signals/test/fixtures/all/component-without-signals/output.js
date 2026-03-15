import { useSignals as _useSignals } from '@preact/signals-react/runtime';
function MyComponent() {
	var _effect = _useSignals(1);
	try {
		return <div>Hello World</div>;
	} finally {
		_effect.f();
	}
}
