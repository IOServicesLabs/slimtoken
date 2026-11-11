export interface Widget {
  id: string;
}

export interface WidgetList {
  items: Widget[];
}

// ERROR is intentionally not exported here: the fixture must fail the
// binding-presence gate for the schema 'Error'.
