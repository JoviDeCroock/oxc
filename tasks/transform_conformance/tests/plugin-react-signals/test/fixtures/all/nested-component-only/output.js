import { useSignals as _useSignals } from '@preact/signals-react/runtime';
describe('suite', () => {
	it('test 2', () => {
		const CountModel = () => signal.value;
		function Counter() {
			var _effect = _useSignals(1);
			try {
				return <div>Hello2</div>;
			} finally {
				_effect.f();
			}
		}
		render(<Counter />);
	});
});
