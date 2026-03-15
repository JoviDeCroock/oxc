import { useSignals as _useSignals } from '@preact/signals-react/runtime';
const MyComponent = () => {
	_useSignals();
	return <div>{name.value}</div>;
};
