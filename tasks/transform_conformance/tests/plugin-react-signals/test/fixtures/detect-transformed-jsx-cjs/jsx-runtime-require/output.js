var _useSignals = require('@preact/signals-react/runtime').useSignals;
const jsxRuntime = require('react/jsx-runtime');
function MyComponent() {
	var _effect = _useSignals(1);
	try {
		signal.value;
		return jsxRuntime.jsx('div', { children: 'Hello World' });
	} finally {
		_effect.f();
	}
}
