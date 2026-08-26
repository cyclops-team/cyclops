// A one-line event bus between the hero's workspace mock and the field
// behind it. When the mock delivers a message from one agent to another,
// the field fires the same pulse between the nodes that stand for them, so
// the two read as one system rather than a picture in front of a texture.

export type SignalListener = (from: number, to: number) => void;

const listeners = new Set<SignalListener>();

export function onSignal(listener: SignalListener): () => void {
	listeners.add(listener);
	return () => {
		listeners.delete(listener);
	};
}

export function emitSignal(from: number, to: number): void {
	for (const listener of listeners) listener(from, to);
}
