import { jsx as _jsx } from 'react/jsx-runtime';
import { useSignals as _useSignals } from '@preact/signals-react/runtime';
function MyComponent() {
	var _effect = _useSignals(1);
	try {
		signal.value;
		return _jsx('div', { children: 'Hello World' });
	} finally {
		_effect.f();
	}
}
