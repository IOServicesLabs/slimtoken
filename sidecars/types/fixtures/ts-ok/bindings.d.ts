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
