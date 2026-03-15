import { useSignals as _useSignals } from 'custom-source';
const MyComponent = () => {
	var _effect = _useSignals(1);
	try {
		signal.value;
		return <div>Hello World</div>;
	} finally {
		_effect.f();
	}
};
