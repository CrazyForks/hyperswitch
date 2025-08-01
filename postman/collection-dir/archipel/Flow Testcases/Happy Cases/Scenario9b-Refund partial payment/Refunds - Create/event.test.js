// Validate status 2xx
pm.test("[POST]::/refunds - Status code is 2xx", function () {
  pm.response.to.be.success;
});

// Validate if response header has matching content-type
pm.test("[POST]::/refunds - Content-Type is application/json", function () {
  pm.expect(pm.response.headers.get("Content-Type")).to.include(
    "application/json",
  );
});

// Set response object as internal variable
let jsonData = {};
try {
  jsonData = pm.response.json();
} catch (e) {}

// pm.collectionVariables - Set refund_id as variable for jsonData.payment_id
if (jsonData?.refund_id) {
  pm.collectionVariables.set("refund_id", jsonData.refund_id);
  console.log(
    "- use {{refund_id}} as collection variable for value",
    jsonData.refund_id,
  );
} else {
  console.log(
    "INFO - Unable to assign variable {{refund_id}}, as jsonData.refund_id is undefined.",
  );
}

// Response body should have value "failed" for "status" and error message should contain "[/refund/{{connector_transaction_id}}] Cannot find any remitted transaction with given order id"
pm.test("[POST]::/refunds - Content check if value for 'status' matches 'failed'", 
function () {
    pm.expect(jsonData.status).to.eql("failed");
    pm.expect(jsonData.error_message).to.include(
      "[/refund/"
    ) + pm.collectionVariables.get("connector_transaction_id") + "] Cannot find any remitted transaction with given order id";
});

// Response body should have value "200" for "amount"
pm.test("[POST]::/refunds - Content check if value for 'amount' matches '200'",
function () {
    pm.expect(jsonData.amount).to.eql(200);
});

// Response body should have "EUR" for "currency"
pm.test("[POST]::/refunds - Content check if value for 'currency' matches 'EUR'",
function () {
    pm.expect(jsonData.currency).to.eql("EUR");
});
