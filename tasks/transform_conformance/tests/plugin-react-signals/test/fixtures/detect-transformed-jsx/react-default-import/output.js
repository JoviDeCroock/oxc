import React from 'react';
import { useSignals as _useSignals } from '@preact/signals-react/runtime';
function MyComponent() {
	var _effect = _useSignals(1);
	try {
		signal.value;
		return React.createElement('div', null, 'Hello World');
	} finally {
		_effect.f();
	}
}
