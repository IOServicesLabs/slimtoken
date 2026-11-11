export interface Widget {
  id: string;
  error?: Error;
}

export interface WidgetList {
  items: Widget[];
}

export interface Error {
  message: string;
}

// A deliberate type error: all three bindings are present (the presence
// gate passes) but tsc must fail on the line below.
export const bad: number = "this is not a number";
